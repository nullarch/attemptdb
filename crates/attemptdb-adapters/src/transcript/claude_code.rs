//! Claude Code transcript parser.
//!
//! Claude Code appends one JSON object per line to
//! `<CLAUDE_CONFIG_DIR>/projects/<slug>/<session-id>.jsonl` (the slug is the
//! working directory with every non-alphanumeric character replaced by `-`).
//! Subagent conversations are written next to it under
//! `<session-id>/subagents/**/agent-<id>.jsonl` with an `agent-<id>.meta.json`
//! (`agentType`, `description`, `toolUseId`) beside each one.
//!
//! Entry shapes verified against Claude Code 2.1.x transcripts:
//!
//! - Conversation entries (`type` = `user` | `assistant`) carry `uuid`,
//!   `parentUuid`, `sessionId` (equal to the file stem), `timestamp` (ISO 8601
//!   with milliseconds, `Z`), `cwd`, `version`, `gitBranch`, `isSidechain`,
//!   `userType`, `entrypoint` and `message { role, content }`. User content is
//!   a string or an array of `text` / `image` /
//!   `tool_result { tool_use_id, content, is_error? }` blocks; assistant
//!   content is an array of `text` / `thinking` / `tool_use { id, name, input }`
//!   blocks written one block per line, all lines of one API message sharing
//!   `message.id`, `message.model`, `message.usage` and `message.stop_reason`
//!   (`tool_use`, `end_turn`, `stop_sequence`, `null` while streaming).
//! - User entries also carry `promptId`, `promptSource` (`typed`, `queued`,
//!   `sdk`, `system`), `isMeta` (context injected by the client, not typed by
//!   a human), `isCompactSummary` (the continuation summary written after a
//!   compaction) and, on tool results, `toolUseResult` (structured output; a
//!   string starting with `Error:` on failures; `interrupted` on shell calls).
//! - `system` entries have a `subtype`: `turn_duration { durationMs }` after
//!   every completed turn, `compact_boundary { compactMetadata { trigger,
//!   preTokens, postTokens } }`, `stop_hook_summary`, `api_error`,
//!   `local_command`, ...
//! - Subagent entries carry `agentId` (the `<id>` of their file) and
//!   `isSidechain: true`; assistant lines add `attributionAgent` (the agent
//!   type). Older builds inlined sidechain entries in the main transcript
//!   without an `agentId`.
//! - Older builds also wrote `summary { summary, leafUuid }` entries
//!   (compaction summaries) without timestamps or session ids.
//! - Everything else (`attachment`, `file-history-snapshot`,
//!   `file-history-delta`, `mode`, `last-prompt`, `ai-title`,
//!   `permission-mode`, `queue-operation`, `bridge-session`, `pr-link`,
//!   `agent-name`, `worktree-state`, `relocated`, `agent-setting`,
//!   `frame-link`, `atis-latch`, `artifact-autoreact-ledger`,
//!   `artifact-comment-monitor`, `progress`) is UI/state bookkeeping: skipped
//!   and counted. Types not listed anywhere become `unknown` events.
//! - A turn cut short appears as a `user` text block
//!   `[Request interrupted by user]` (or `... for tool use`); a rejected tool
//!   call as a `tool_result` with `is_error: true` whose text says the user
//!   does not want to proceed.
//!
//! The parser is pure: it never touches the file system, never panics on
//! malformed input (bad lines are counted and reported as warnings), and
//! produces events whose ids are a function of `(session id, entry uuid,
//! block index)` only.

use crate::CaptureContext;
use crate::common::{Normaliser, Payload, TOOL_OUTPUT_LIMIT, UNKNOWN_SESSION, is_token, to_snake};
use attemptdb_core::event::Provider;
use attemptdb_core::{CaptureMode, Event, EventId, EventKind, Outcome, OutcomeStatus, Timestamp};
use serde_json::{Map, Value};
use std::collections::HashMap;

/// Value of `attrs.reconstructed_from` on every event produced here.
pub const RECONSTRUCTED_FROM: &str = "claude_code_transcript";

/// Prefix of every `provider_event_name` and of every derived event id.
const NAME_PREFIX: &str = "transcript";

/// How many leading lines may lack a `sessionId` before the parser falls
/// back to the session hint. Old-format files start with `summary` entries
/// that carry no session id.
const SESSION_PEEK_LINES: usize = 256;

/// Warnings kept per file; the rest are counted.
const MAX_WARNINGS: usize = 200;

/// Known entry types that carry no observable fact of their own.
const SKIPPED_TYPES: &[&str] = &[
    "attachment",
    "file-history-snapshot",
    "file-history-delta",
    "mode",
    "last-prompt",
    "ai-title",
    "permission-mode",
    "queue-operation",
    "bridge-session",
    "pr-link",
    "agent-name",
    "worktree-state",
    "relocated",
    "agent-setting",
    "frame-link",
    "progress",
    "atis-latch",
    "artifact-autoreact-ledger",
    "artifact-comment-monitor",
];

/// User text injected by the client rather than typed by a human (shared
/// with the hook adapter).
const INJECTED_PREFIXES: &[&str] = crate::common::INJECTED_PROMPT_PREFIXES;

const INTERRUPTED_PREFIX: &str = "[Request interrupted by user";

/// Parser options. `include_content` normally mirrors the capture mode.
#[derive(Clone, Debug)]
pub struct TranscriptOptions {
    /// Keep content-bearing fields (prompts, commands, tool output, agent
    /// messages). `false` yields metadata-only events regardless of the
    /// capture mode in the context.
    pub include_content: bool,
    /// Largest tool output retained, in bytes (never more than
    /// [`TOOL_OUTPUT_LIMIT`]).
    pub max_tool_output: usize,
    /// Session id to use when no entry carries one (the file stem).
    pub session_id_hint: Option<String>,
    /// Agent type of a subagent transcript (from `agent-<id>.meta.json`).
    pub agent_type_hint: Option<String>,
    /// The parent's `Task`/`Agent` tool-use id that spawned a subagent
    /// transcript (from `agent-<id>.meta.json`).
    pub parent_tool_use_id: Option<String>,
}

impl Default for TranscriptOptions {
    fn default() -> Self {
        Self {
            include_content: true,
            max_tool_output: TOOL_OUTPUT_LIMIT,
            session_id_hint: None,
            agent_type_hint: None,
            parent_tool_use_id: None,
        }
    }
}

impl TranscriptOptions {
    /// Options whose content policy follows a capture mode.
    pub fn for_capture_mode(mode: CaptureMode) -> Self {
        Self {
            include_content: mode.persists_content_locally(),
            ..Self::default()
        }
    }
}

/// Counts describing one parsed transcript.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct TranscriptStats {
    /// JSON object lines seen.
    pub entries: usize,
    pub prompts: usize,
    pub tool_calls: usize,
    pub tool_failures: usize,
    /// Turn ends emitted (synthesised stops and interruptions).
    pub turns: usize,
    /// Entries attributed to a subagent / sidechain.
    pub subagent_entries: usize,
    /// Entries of a type this parser does not know (emitted as `unknown`).
    pub unknown_entries: usize,
    /// Known-but-uninteresting entries (bookkeeping, thinking-only lines).
    pub skipped_entries: usize,
    /// Lines that were not a JSON object.
    pub malformed_lines: usize,
}

/// Result of parsing one transcript file.
#[derive(Debug)]
pub struct TranscriptImport {
    pub events: Vec<Event>,
    /// The session id the events were attributed to, when one was found.
    pub provider_session_id: Option<String>,
    pub stats: TranscriptStats,
    pub warnings: Vec<String>,
}

/// Parse one Claude Code transcript (main session or subagent file) into
/// reconstructed events. `ctx.project` is used as-is; the transcript's
/// `gitBranch` fills `project.branch` only when the context has none.
pub fn parse_claude_transcript(
    lines: impl Iterator<Item = String>,
    ctx: &CaptureContext,
    opts: &TranscriptOptions,
) -> TranscriptImport {
    let mut p = Parser::new(ctx, opts);
    let mut buffered: Vec<(usize, String)> = Vec::new();
    let mut line_no = 0usize;
    for line in lines {
        line_no += 1;
        if p.session.is_none() {
            match peek_session_id(&line) {
                Some(sid) => {
                    p.session = Some(sid);
                    p.replay(&mut buffered);
                }
                None => {
                    buffered.push((line_no, line));
                    if buffered.len() >= SESSION_PEEK_LINES {
                        p.resolve_session_fallback();
                        p.replay(&mut buffered);
                    }
                    continue;
                }
            }
        }
        p.process_line(line_no, &line);
    }
    if p.session.is_none() {
        p.resolve_session_fallback();
    }
    p.replay(&mut buffered);
    p.finish()
}

// ---------------------------------------------------------------------------
// Entry views
// ---------------------------------------------------------------------------

/// One parsed transcript line.
struct Entry {
    map: Map<String, Value>,
    kind: String,
    line_no: usize,
}

impl Entry {
    fn get(&self, key: &str) -> Option<&Value> {
        self.map.get(key)
    }

    fn str(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
    }

    fn bool(&self, key: &str) -> Option<bool> {
        self.get(key).and_then(Value::as_bool)
    }

    fn message(&self) -> Option<&Map<String, Value>> {
        self.get("message").and_then(Value::as_object)
    }

    fn content(&self) -> Option<&Value> {
        self.message().and_then(|m| m.get("content"))
    }

    /// The content-free scalars needed to build events from this entry,
    /// cheap to keep after the entry itself is gone.
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            kind: self.kind.clone(),
            id: self
                .str("uuid")
                .or_else(|| self.str("leafUuid"))
                .map(str::to_string),
            ts: self
                .str("timestamp")
                .filter(|t| Timestamp::parse(t).is_some())
                .map(str::to_string),
            cwd: self.str("cwd").map(str::to_string),
            version: self.str("version").map(str::to_string),
            git_branch: self.str("gitBranch").map(str::to_string),
            entrypoint: self.str("entrypoint").filter(|s| is_token(s)).map(str::to_string),
            line_no: self.line_no,
        }
    }
}

#[derive(Clone, Debug)]
struct Snapshot {
    kind: String,
    id: Option<String>,
    ts: Option<String>,
    cwd: Option<String>,
    version: Option<String>,
    git_branch: Option<String>,
    entrypoint: Option<String>,
    line_no: usize,
}

impl Snapshot {
    /// The stable identity of the entry used in derived event ids.
    fn id_part(&self) -> String {
        self.id
            .clone()
            .unwrap_or_else(|| format!("line:{}", self.line_no))
    }
}

/// Which subagent / sidechain an entry belongs to.
#[derive(Clone, Debug)]
struct SideRef {
    key: String,
    agent_type: Option<String>,
}

struct SideState {
    agent_type: Option<String>,
    last: Snapshot,
    started: bool,
}

struct ToolUse {
    name: String,
    input: Value,
}

/// An assistant text message waiting to learn what follows it.
struct Pending {
    snap: Snapshot,
    msg_id: Option<String>,
    text: String,
    model: Option<String>,
    stop_reason: Option<String>,
    output_tokens: Option<u64>,
    side: Option<SideRef>,
}

enum UserText {
    Prompt(&'static str),
    Injected,
    CompactSummary,
    Interrupted { for_tool_use: bool },
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

struct Parser<'a> {
    ctx: &'a CaptureContext,
    opts: &'a TranscriptOptions,
    events: Vec<Event>,
    stats: TranscriptStats,
    warnings: Vec<String>,
    suppressed_warnings: usize,
    session: Option<String>,
    session_started: Option<Event>,
    session_warned: bool,
    /// Last timestamp text seen, for entries that carry none.
    last_ts: Option<String>,
    /// Prompts seen so far in the main chain.
    turn_index: u64,
    provider_turn_id: Option<String>,
    /// `durationMs` of the last `turn_duration` entry, attached to the next
    /// synthesised turn end.
    pending_duration: Option<u64>,
    tools: HashMap<String, ToolUse>,
    pending: Option<Pending>,
    sides: HashMap<String, SideState>,
    side_order: Vec<String>,
    /// Key of the current run of inline sidechain entries without `agentId`.
    inline_run: Option<String>,
}

impl<'a> Parser<'a> {
    fn new(ctx: &'a CaptureContext, opts: &'a TranscriptOptions) -> Self {
        Self {
            ctx,
            opts,
            events: Vec::new(),
            stats: TranscriptStats::default(),
            warnings: Vec::new(),
            suppressed_warnings: 0,
            session: None,
            session_started: None,
            session_warned: false,
            last_ts: None,
            turn_index: 0,
            provider_turn_id: None,
            pending_duration: None,
            tools: HashMap::new(),
            pending: None,
            sides: HashMap::new(),
            side_order: Vec::new(),
            inline_run: None,
        }
    }

    // --- diagnostics --------------------------------------------------------

    fn warn(&mut self, line_no: usize, message: &str) {
        if self.warnings.len() < MAX_WARNINGS {
            self.warnings.push(format!("line {line_no}: {message}"));
        } else {
            self.suppressed_warnings += 1;
        }
    }

    fn malformed(&mut self, line_no: usize, message: &str) {
        self.stats.malformed_lines += 1;
        self.warn(line_no, message);
    }

    fn resolve_session_fallback(&mut self) {
        match &self.opts.session_id_hint {
            Some(hint) => {
                self.warnings.push(format!(
                    "no sessionId in the first {SESSION_PEEK_LINES} entries; using the file name {hint:?}"
                ));
                self.session = Some(hint.clone());
            }
            None => {
                self.warnings.push(format!(
                    "no sessionId in the first {SESSION_PEEK_LINES} entries and no file name hint; events attributed to session {UNKNOWN_SESSION:?}"
                ));
                self.session = Some(UNKNOWN_SESSION.to_string());
            }
        }
    }

    fn session(&self) -> String {
        self.session
            .clone()
            .unwrap_or_else(|| UNKNOWN_SESSION.to_string())
    }

    fn check_session(&mut self, entry: &Entry) {
        if self.session_warned {
            return;
        }
        if let Some(sid) = entry.str("sessionId")
            && self.session.as_deref() != Some(sid)
        {
            self.session_warned = true;
            self.warn(
                entry.line_no,
                "sessionId differs from the file's session; events are attributed to the file's session",
            );
        }
    }

    // --- line dispatch ------------------------------------------------------

    fn replay(&mut self, buffered: &mut Vec<(usize, String)>) {
        for (line_no, line) in buffered.drain(..) {
            self.process_line(line_no, &line);
        }
    }

    fn process_line(&mut self, line_no: usize, line: &str) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }
        let value: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(e) => {
                self.malformed(line_no, &format!("invalid JSON ({e})"));
                return;
            }
        };
        let Value::Object(map) = value else {
            self.malformed(line_no, "not a JSON object");
            return;
        };
        self.stats.entries += 1;
        let kind = map
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let entry = Entry { map, kind, line_no };
        let snap = entry.snapshot();
        if let Some(ts) = &snap.ts {
            self.last_ts = Some(ts.clone());
        }
        self.check_session(&entry);
        if entry.kind.is_empty() {
            self.warn(line_no, "entry has no `type`");
            self.unknown(&snap, "untyped", None);
            return;
        }
        let side = self.touch_sidechain(&entry, &snap);
        match &side {
            Some(_) => self.stats.subagent_entries += 1,
            None => self.ensure_session_started(&snap),
        }
        match entry.kind.as_str() {
            "user" => self.user(&entry, &snap, side.as_ref()),
            "assistant" => self.assistant(&entry, &snap, side.as_ref()),
            "system" => self.system(&entry, &snap, side.as_ref()),
            "summary" => self.summary(&entry, &snap),
            t if SKIPPED_TYPES.contains(&t) => self.stats.skipped_entries += 1,
            t => {
                let t = t.to_string();
                self.unknown(&snap, &t, side.as_ref());
            }
        }
    }

    // --- event construction -------------------------------------------------

    /// Build one event from an entry snapshot. Every reconstructed event goes
    /// through here so the provenance attributes, id derivation and content
    /// policy are applied uniformly.
    fn emit(
        &mut self,
        snap: &Snapshot,
        name: &str,
        kind: EventKind,
        suffix: &str,
        side: Option<&SideRef>,
        fill: impl FnOnce(&mut Normaliser<'_>),
    ) {
        let session = self.session();
        let mut synth = Map::new();
        if let Some(cwd) = &snap.cwd {
            synth.insert("cwd".into(), Value::String(cwd.clone()));
        }
        if let Some(ts) = snap.ts.as_ref().or(self.last_ts.as_ref()) {
            synth.insert("timestamp".into(), Value::String(ts.clone()));
        }
        let synth = Value::Object(synth);
        let payload = Payload::from_value(&synth).expect("synthetic payload is an object");
        let mut n = Normaliser::new(self.ctx, payload, Provider::ClaudeCode, name, kind, &session);
        n.set_cwd();
        n.attr("reconstructed", true);
        n.attr("reconstructed_from", RECONSTRUCTED_FROM);
        n.attr("transcript_entry_type", snap.kind.as_str());
        n.attr("transcript_present", true);
        match side {
            Some(s) => {
                n.set_subagent(&s.key, s.agent_type.as_deref());
                n.attr("is_sidechain", true);
            }
            None => n.attr("turn_index_hint", self.turn_index),
        }
        fill(&mut n);
        let mut ev = n.finish();
        ev.raw = None;
        ev.hook_version = None;
        ev.provider_version = snap.version.clone();
        if ev.project.branch.is_none() {
            ev.project.branch = snap.git_branch.clone();
        }
        if side.is_none() {
            ev.provider_turn_id = self.provider_turn_id.clone();
        }
        ev.event_id = EventId::derive(&[NAME_PREFIX, &session, &snap.id_part(), suffix]);
        ev.attrs.remove("hook_event_name");
        if !self.opts.include_content {
            ev.content = None;
        }
        self.events.push(ev);
    }

    fn ensure_session_started(&mut self, snap: &Snapshot) {
        if self.session_started.is_some() || snap.ts.is_none() {
            return;
        }
        let entrypoint = snap.entrypoint.clone();
        self.emit(
            snap,
            "transcript:session_start",
            EventKind::SessionStarted,
            "session_started",
            None,
            move |n| {
                n.attr("source", "transcript");
                if let Some(e) = entrypoint {
                    n.provider_attr("entrypoint", e);
                }
            },
        );
        self.session_started = self.events.pop();
    }

    // --- sidechains / subagents ---------------------------------------------

    fn touch_sidechain(&mut self, entry: &Entry, snap: &Snapshot) -> Option<SideRef> {
        let agent_id = entry.str("agentId");
        if agent_id.is_none() && entry.bool("isSidechain") != Some(true) {
            self.inline_run = None;
            return None;
        }
        let key = match agent_id {
            Some(id) => id.to_string(),
            None => self
                .inline_run
                .get_or_insert_with(|| format!("sidechain:{}", snap.id_part()))
                .clone(),
        };
        let agent_type = entry
            .str("attributionAgent")
            .filter(|s| is_token(s))
            .map(str::to_string)
            .or_else(|| self.opts.agent_type_hint.clone());
        if !self.sides.contains_key(&key) {
            self.side_order.push(key.clone());
            self.sides.insert(
                key.clone(),
                SideState {
                    agent_type: agent_type.clone(),
                    last: snap.clone(),
                    started: false,
                },
            );
        }
        let state = self.sides.get_mut(&key).expect("inserted above");
        if state.agent_type.is_none() {
            state.agent_type = agent_type;
        }
        state.last = snap.clone();
        Some(SideRef {
            key,
            agent_type: state.agent_type.clone(),
        })
    }

    fn subagent_started(&self, key: &str) -> bool {
        self.sides.get(key).is_some_and(|s| s.started)
    }

    fn ensure_subagent_started(&mut self, snap: &Snapshot, side: &SideRef, prompt: Option<&str>) {
        let Some(state) = self.sides.get_mut(&side.key) else {
            return;
        };
        if state.started {
            return;
        }
        state.started = true;
        let prompt = prompt.map(str::to_string);
        let parent = self.opts.parent_tool_use_id.clone();
        self.emit(
            snap,
            "transcript:subagent_start",
            EventKind::SubagentStarted,
            "subagent_started",
            Some(side),
            move |n| {
                if let Some(p) = prompt {
                    n.set_prompt(&p);
                }
                if let Some(id) = parent {
                    n.provider_attr("parent_tool_use_id", id);
                }
            },
        );
    }

    // --- user entries -------------------------------------------------------

    fn user(&mut self, entry: &Entry, snap: &Snapshot, side: Option<&SideRef>) {
        let content = entry.content();
        let results = tool_result_blocks(content);
        if !results.is_empty() {
            if let Some(s) = side {
                self.ensure_subagent_started(snap, s, None);
            }
            self.settle_pending(side);
            let tool_use_result = entry.get("toolUseResult");
            for (index, block) in results {
                self.tool_result(snap, side, index, block, tool_use_result);
            }
            return;
        }
        let text = user_text(content);
        let image_count = count_blocks(content, "image");
        match classify_user_text(entry, text.as_deref(), image_count) {
            UserText::Interrupted { for_tool_use } => {
                self.flush_pending(false);
                match side {
                    None => self.turn_interrupted(snap, for_tool_use),
                    Some(_) => self.stats.skipped_entries += 1,
                }
            }
            UserText::CompactSummary | UserText::Injected => {
                self.flush_pending(true);
                self.stats.skipped_entries += 1;
            }
            UserText::Prompt(prompt_kind) => {
                self.flush_pending(true);
                match side {
                    Some(s) if !self.subagent_started(&s.key) => {
                        self.ensure_subagent_started(snap, s, text.as_deref());
                    }
                    Some(_) => self.stats.skipped_entries += 1,
                    None => self.prompt(entry, snap, text.unwrap_or_default(), prompt_kind, image_count),
                }
            }
        }
    }

    fn prompt(
        &mut self,
        entry: &Entry,
        snap: &Snapshot,
        text: String,
        prompt_kind: &'static str,
        image_count: usize,
    ) {
        self.turn_index += 1;
        self.provider_turn_id = entry.str("promptId").map(str::to_string);
        self.pending_duration = None;
        self.stats.prompts += 1;
        let source = entry
            .str("promptSource")
            .filter(|s| is_token(s))
            .map(str::to_string);
        let entrypoint = snap.entrypoint.clone();
        self.emit(
            snap,
            "transcript:user",
            EventKind::PromptSubmitted,
            "entry",
            None,
            move |n| {
                n.set_prompt(&text);
                n.provider_attr("prompt_kind", prompt_kind);
                if let Some(s) = source {
                    n.provider_attr("prompt_source", s);
                }
                if let Some(e) = entrypoint {
                    n.provider_attr("entrypoint", e);
                }
                if image_count > 0 {
                    n.provider_attr("image_count", image_count as u64);
                }
            },
        );
    }

    fn turn_interrupted(&mut self, snap: &Snapshot, for_tool_use: bool) {
        self.pending_duration = None;
        self.stats.turns += 1;
        self.emit(
            snap,
            "transcript:user:interrupted",
            EventKind::TurnStopped,
            "entry",
            None,
            move |n| {
                n.attr("reason", "user_interrupt");
                n.provider_attr(
                    "interrupt_kind",
                    if for_tool_use { "tool_use" } else { "turn" },
                );
                n.event.outcome = Some(Outcome {
                    status: OutcomeStatus::Cancelled,
                    class: Some("interrupted".to_string()),
                    exit_code: None,
                });
            },
        );
    }

    fn tool_result(
        &mut self,
        snap: &Snapshot,
        side: Option<&SideRef>,
        index: usize,
        block: &Map<String, Value>,
        tool_use_result: Option<&Value>,
    ) {
        let call_id = block
            .get("tool_use_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let known = self
            .tools
            .get(&call_id)
            .map(|t| (t.name.clone(), t.input.clone()));
        let text = result_text(block);
        let is_error = block.get("is_error").and_then(Value::as_bool) == Some(true);
        let interrupted = tool_use_result
            .and_then(|v| v.get("interrupted"))
            .and_then(Value::as_bool)
            == Some(true);
        let rejected = is_error && is_user_rejection(&text, tool_use_result);
        let kind = if is_error || interrupted {
            self.stats.tool_failures += 1;
            EventKind::ToolCallFailed
        } else {
            EventKind::ToolCallFinished
        };
        let (bounded, truncated) = bound_text(&text, self.opts.max_tool_output);
        let suffix = format!("block:{index}");
        self.emit(
            snap,
            "transcript:user:tool_result",
            kind,
            &suffix,
            side,
            move |n| {
                let (tool_name, input) = match known {
                    Some((name, input)) => (name, Some(input)),
                    None => ("unknown".to_string(), None),
                };
                n.set_tool(&tool_name, Some(&call_id));
                if let Some(input) = &input {
                    n.apply_tool_input(input);
                }
                if !bounded.is_empty() {
                    if truncated {
                        n.attr("tool_output_truncated", true);
                    }
                    n.set_tool_output(&Value::String(bounded));
                }
                let error_text = (!text.is_empty()).then_some(text.as_str());
                if rejected {
                    n.set_failure_with_class("user_rejected", error_text);
                    if let Some(o) = n.event.outcome.as_mut() {
                        o.status = OutcomeStatus::Denied;
                    }
                } else if is_error {
                    n.set_failure(error_text, None);
                } else if interrupted {
                    n.set_failure_with_class("interrupted", None);
                    if let Some(o) = n.event.outcome.as_mut() {
                        o.status = OutcomeStatus::Cancelled;
                    }
                } else {
                    n.set_success(None);
                }
            },
        );
    }

    // --- assistant entries --------------------------------------------------

    fn assistant(&mut self, entry: &Entry, snap: &Snapshot, side: Option<&SideRef>) {
        if let Some(s) = side {
            self.ensure_subagent_started(snap, s, None);
        }
        let Some(blocks) = entry.content().and_then(Value::as_array) else {
            self.stats.skipped_entries += 1;
            return;
        };
        let message = entry.message();
        let model = message
            .and_then(|m| m.get("model"))
            .and_then(Value::as_str)
            .filter(|s| is_token(s));
        let msg_id = message.and_then(|m| m.get("id")).and_then(Value::as_str);
        let stop_reason = message
            .and_then(|m| m.get("stop_reason"))
            .and_then(Value::as_str)
            .filter(|s| is_token(s));
        let output_tokens = message
            .and_then(|m| m.get("usage"))
            .and_then(|u| u.get("output_tokens"))
            .and_then(Value::as_u64);

        let mut texts: Vec<&str> = Vec::new();
        let mut had_tool_use = false;
        for (index, block) in blocks.iter().enumerate() {
            let Some(obj) = block.as_object() else { continue };
            match obj.get("type").and_then(Value::as_str) {
                Some("tool_use") => {
                    if !had_tool_use {
                        had_tool_use = true;
                        self.settle_pending(side);
                    }
                    self.tool_use(snap, side, index, obj, model);
                }
                Some("text") => {
                    if let Some(t) = obj.get("text").and_then(Value::as_str) {
                        texts.push(t);
                    }
                }
                _ => {}
            }
        }
        if had_tool_use {
            return;
        }
        if texts.is_empty() {
            // Thinking-only (or empty) line: private reasoning is never kept.
            self.stats.skipped_entries += 1;
            return;
        }
        let text = texts.join("\n");
        if let Some(p) = self.pending.as_mut()
            && p.msg_id.is_some()
            && p.msg_id.as_deref() == msg_id
            && same_side(p.side.as_ref(), side)
        {
            // Another text block of the same API message.
            p.text.push('\n');
            p.text.push_str(&text);
            if stop_reason.is_some() {
                p.stop_reason = stop_reason.map(str::to_string);
            }
            if output_tokens.is_some() {
                p.output_tokens = output_tokens;
            }
            return;
        }
        // A different message follows an unflushed text on the same chain:
        // the earlier text was interim narration, keep only the newest.
        self.pending = Some(Pending {
            snap: snap.clone(),
            msg_id: msg_id.map(str::to_string),
            text,
            model: model.map(str::to_string),
            stop_reason: stop_reason.map(str::to_string),
            output_tokens,
            side: side.cloned(),
        });
    }

    fn tool_use(
        &mut self,
        snap: &Snapshot,
        side: Option<&SideRef>,
        index: usize,
        block: &Map<String, Value>,
        model: Option<&str>,
    ) {
        let name = block
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("unknown")
            .to_string();
        let id = block
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let input = block
            .get("input")
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));
        if let Some(id) = &id {
            self.tools.insert(
                id.clone(),
                ToolUse {
                    name: name.clone(),
                    input: input.clone(),
                },
            );
        }
        self.stats.tool_calls += 1;
        let model = model.map(str::to_string);
        let suffix = format!("block:{index}");
        self.emit(
            snap,
            "transcript:assistant:tool_use",
            EventKind::ToolCallStarted,
            &suffix,
            side,
            move |n| {
                n.set_tool(&name, id.as_deref());
                n.apply_tool_input(&input);
                n.event.agent.model = model;
            },
        );
    }

    /// A tool call (or its result) follows the pending assistant text. On the
    /// same chain the text was interim narration and is dropped; on another
    /// chain the text is kept as a message (its own chain simply went quiet).
    fn settle_pending(&mut self, side: Option<&SideRef>) {
        let Some(p) = self.pending.as_ref() else { return };
        if same_side(p.side.as_ref(), side) {
            self.pending = None;
        } else {
            self.flush_pending(false);
        }
    }

    /// Emit the pending assistant text as an `AgentMessage` and, when it was
    /// the end of a main-chain turn, a synthesised `TurnStopped`.
    fn flush_pending(&mut self, synth_stop: bool) {
        let Some(p) = self.pending.take() else {
            return;
        };
        let chars = p.text.chars().count() as u64;
        let side = p.side.clone();
        let stop_reason = p.stop_reason.clone();
        let stop_reason_for_turn = p.stop_reason.clone();
        let (text, model, output_tokens) = (p.text, p.model, p.output_tokens);
        self.emit(
            &p.snap,
            "transcript:assistant:text",
            EventKind::AgentMessage,
            "entry",
            side.as_ref(),
            move |n| {
                n.set_message(&text);
                n.provider_attr("message_chars", chars);
                if let Some(s) = stop_reason {
                    n.provider_attr("stop_reason", s);
                }
                if let Some(t) = output_tokens {
                    n.provider_attr("output_tokens", t);
                }
                n.event.agent.model = model;
            },
        );
        if synth_stop && side.is_none() {
            let duration = self.pending_duration.take();
            self.stats.turns += 1;
            self.emit(
                &p.snap,
                "transcript:turn_end",
                EventKind::TurnStopped,
                "turn_end",
                None,
                move |n| {
                    n.event.duration_ms = duration;
                    if let Some(s) = stop_reason_for_turn {
                        n.provider_attr("stop_reason", s);
                    }
                },
            );
        }
    }

    // --- system / summary / unknown -----------------------------------------

    fn system(&mut self, entry: &Entry, snap: &Snapshot, side: Option<&SideRef>) {
        match entry.str("subtype") {
            Some("compact_boundary") => {
                let meta = entry.get("compactMetadata").and_then(Value::as_object);
                let trigger = meta
                    .and_then(|m| m.get("trigger"))
                    .and_then(Value::as_str)
                    .filter(|t| is_token(t))
                    .map(to_snake)
                    .unwrap_or_else(|| "transcript_compact_boundary".to_string());
                let pre = meta.and_then(|m| m.get("preTokens")).and_then(Value::as_u64);
                let post = meta.and_then(|m| m.get("postTokens")).and_then(Value::as_u64);
                self.emit(
                    snap,
                    "transcript:system:compact_boundary",
                    EventKind::CompactionFinished,
                    "entry",
                    side,
                    move |n| {
                        n.attr("trigger", trigger);
                        if let Some(t) = pre {
                            n.provider_attr("pre_tokens", t);
                        }
                        if let Some(t) = post {
                            n.provider_attr("post_tokens", t);
                        }
                    },
                );
            }
            Some("turn_duration") => {
                self.pending_duration = entry.get("durationMs").and_then(Value::as_u64);
                self.stats.skipped_entries += 1;
            }
            _ => self.stats.skipped_entries += 1,
        }
    }

    fn summary(&mut self, entry: &Entry, snap: &Snapshot) {
        let text = entry.str("summary").map(str::to_string);
        self.emit(
            snap,
            "transcript:summary",
            EventKind::CompactionFinished,
            "entry",
            None,
            move |n| {
                n.attr("trigger", "transcript_summary");
                if let Some(t) = text {
                    n.set_extra("summary", t);
                }
            },
        );
    }

    fn unknown(&mut self, snap: &Snapshot, entry_type: &str, side: Option<&SideRef>) {
        self.stats.unknown_entries += 1;
        let name = format!("{NAME_PREFIX}:{entry_type}");
        self.emit(snap, &name, EventKind::Unknown, "entry", side, |_| {});
    }

    // --- finish -------------------------------------------------------------

    fn finish(mut self) -> TranscriptImport {
        self.flush_pending(true);
        for key in std::mem::take(&mut self.side_order) {
            let Some(state) = self.sides.remove(&key) else {
                continue;
            };
            if !state.started {
                continue;
            }
            let side = SideRef {
                key,
                agent_type: state.agent_type,
            };
            self.emit(
                &state.last,
                "transcript:subagent_stop",
                EventKind::SubagentStopped,
                "subagent_stopped",
                Some(&side),
                |_| {},
            );
        }
        if let Some(start) = self.session_started.take() {
            self.events.insert(0, start);
        }
        if self.suppressed_warnings > 0 {
            self.warnings.push(format!(
                "{} further warning(s) suppressed",
                self.suppressed_warnings
            ));
        }
        let provider_session_id = self.session.filter(|s| s != UNKNOWN_SESSION);
        TranscriptImport {
            events: self.events,
            provider_session_id,
            stats: self.stats,
            warnings: self.warnings,
        }
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

fn same_side(a: Option<&SideRef>, b: Option<&SideRef>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => x.key == y.key,
        _ => false,
    }
}

/// `sessionId` of a line, when it is a JSON object carrying one.
fn peek_session_id(line: &str) -> Option<String> {
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    value
        .get("sessionId")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// `(index, block)` of every `tool_result` block in a user content array.
fn tool_result_blocks(content: Option<&Value>) -> Vec<(usize, &Map<String, Value>)> {
    content
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .enumerate()
                .filter_map(|(i, b)| b.as_object().map(|o| (i, o)))
                .filter(|(_, o)| o.get("type").and_then(Value::as_str) == Some("tool_result"))
                .collect()
        })
        .unwrap_or_default()
}

/// The human-readable text of a user entry: the content string, or the text
/// blocks of a content array joined by newlines.
fn user_text(content: Option<&Value>) -> Option<String> {
    match content? {
        Value::String(s) => (!s.is_empty()).then(|| s.clone()),
        Value::Array(blocks) => {
            let texts: Vec<&str> = blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .filter(|t| !t.is_empty())
                .collect();
            (!texts.is_empty()).then(|| texts.join("\n"))
        }
        _ => None,
    }
}

fn count_blocks(content: Option<&Value>, block_type: &str) -> usize {
    content
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some(block_type))
                .count()
        })
        .unwrap_or(0)
}

/// Text of a `tool_result` block: a string, or its text sub-blocks joined.
fn result_text(block: &Map<String, Value>) -> String {
    match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Whether an errored tool result is the user declining the call.
fn is_user_rejection(text: &str, tool_use_result: Option<&Value>) -> bool {
    let lower = text.to_ascii_lowercase();
    if lower.contains("user doesn't want to proceed")
        || lower.contains("user rejected")
        || lower.contains("tool use was rejected")
    {
        return true;
    }
    tool_use_result
        .and_then(Value::as_str)
        .is_some_and(|s| s.to_ascii_lowercase().contains("user rejected"))
}

/// Bound a text to `max` bytes on a character boundary.
fn bound_text(text: &str, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text.to_string(), false);
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_string(), true)
}

fn classify_user_text(entry: &Entry, text: Option<&str>, image_count: usize) -> UserText {
    if entry.bool("isCompactSummary") == Some(true) {
        return UserText::CompactSummary;
    }
    let t = text.map(str::trim_start).unwrap_or("");
    if t.starts_with(INTERRUPTED_PREFIX) {
        return UserText::Interrupted {
            for_tool_use: t.contains("for tool use"),
        };
    }
    if entry.bool("isMeta") == Some(true) || entry.str("promptSource") == Some("system") {
        return UserText::Injected;
    }
    if INJECTED_PREFIXES.iter().any(|p| t.starts_with(p)) {
        return UserText::Injected;
    }
    if t.starts_with("<command-name>") || t.starts_with("<command-message>") {
        return UserText::Prompt("slash_command");
    }
    if t.starts_with("<bash-input>") {
        return UserText::Prompt("bash_input");
    }
    if t.is_empty() {
        return if image_count > 0 {
            UserText::Prompt("image")
        } else {
            UserText::Injected
        };
    }
    UserText::Prompt("text")
}

#[cfg(test)]
mod unit {
    use super::*;

    fn entry(json: &str) -> Entry {
        let Value::Object(map) = serde_json::from_str(json).unwrap() else {
            panic!("object")
        };
        let kind = map
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        Entry {
            map,
            kind,
            line_no: 1,
        }
    }

    #[test]
    fn user_text_classification() {
        let plain = entry(r#"{"type":"user","promptSource":"typed"}"#);
        assert!(matches!(
            classify_user_text(&plain, Some("hello?"), 0),
            UserText::Prompt("text")
        ));
        assert!(matches!(
            classify_user_text(&plain, Some("<command-name>/compact</command-name>"), 0),
            UserText::Prompt("slash_command")
        ));
        assert!(matches!(
            classify_user_text(&plain, Some("<local-command-stdout>x</local-command-stdout>"), 0),
            UserText::Injected
        ));
        assert!(matches!(
            classify_user_text(&plain, Some("[Request interrupted by user for tool use]"), 0),
            UserText::Interrupted { for_tool_use: true }
        ));
        assert!(matches!(
            classify_user_text(&plain, None, 2),
            UserText::Prompt("image")
        ));
        assert!(matches!(classify_user_text(&plain, None, 0), UserText::Injected));
        let meta = entry(r#"{"type":"user","isMeta":true}"#);
        assert!(matches!(classify_user_text(&meta, Some("Please analyze"), 0), UserText::Injected));
        let compact = entry(r#"{"type":"user","isCompactSummary":true}"#);
        assert!(matches!(
            classify_user_text(&compact, Some("This session is being continued"), 0),
            UserText::CompactSummary
        ));
    }

    #[test]
    fn result_text_and_rejection() {
        let block: Map<String, Value> = serde_json::from_str(
            r#"{"type":"tool_result","content":[{"type":"text","text":"a"},{"type":"image"},{"type":"text","text":"b"}]}"#,
        )
        .unwrap();
        assert_eq!(result_text(&block), "a\nb");
        assert!(is_user_rejection("The user doesn't want to proceed with this tool use.", None));
        assert!(is_user_rejection("", Some(&Value::String("User rejected tool use".into()))));
        assert!(!is_user_rejection("Exit code 1", Some(&serde_json::json!({"stdout": ""}))));
    }

    #[test]
    fn bounding_respects_char_boundaries() {
        let (s, truncated) = bound_text("한글한글", 4);
        assert!(truncated);
        assert_eq!(s, "한");
        assert_eq!(bound_text("abc", 3), ("abc".to_string(), false));
    }

    #[test]
    fn peeks_session_id() {
        assert_eq!(peek_session_id(r#"{"sessionId":"s-1"}"#).as_deref(), Some("s-1"));
        assert_eq!(peek_session_id(r#"{"type":"summary"}"#), None);
        assert_eq!(peek_session_id("garbage"), None);
    }
}
