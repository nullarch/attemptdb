//! Codex rollout parser.
//!
//! Codex CLI (and the Codex desktop and exec front ends) append one JSON
//! object per line to `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<timestamp>-<thread-uuid>.jsonl`.
//! Rollouts are large: the owner's machine holds 456 files, 8.2 GB, the
//! biggest 595 MB, with single lines up to 12 MB (generated images,
//! screenshots, compaction snapshots). The parser therefore *streams*: it
//! reads one line at a time through a bounded buffer, hands every event to a
//! callback as soon as it is built, and keeps only bounded state (the open
//! tool calls and a few recent messages).
//!
//! Shapes below were read from Codex 0.130 through 0.160 rollouts (structure
//! only; names and counts, never content). Every line is an envelope
//! `{"timestamp": ISO8601, ["ordinal": n,] "type": T, "payload": {...}}`;
//! `ordinal` exists from 0.135 on. The first line is always `session_meta`.
//!
//! - `session_meta` { `id` (this thread), `session_id` (the root session; equals
//!   `id` unless this is a subagent thread), `cwd`, `cli_version`, `originator`,
//!   `source` (a string such as `cli`, `exec`, `vscode`, or an object
//!   `{"subagent": ...}`), `git { branch, commit_hash, repository_url }`,
//!   `parent_thread_id`, `forked_from_id`, `subagent_history_start_ordinal`,
//!   `agent_role`, ... }. A subagent rollout starts with a copy of its
//!   parent's history; lines whose `ordinal` is below
//!   `subagent_history_start_ordinal` are that inherited prefix and are
//!   skipped (the parent's own rollout holds them).
//! - `turn_context` { `turn_id`, `cwd`, `model`, `approval_policy`, `effort`,
//!   ... } and `event_msg/task_started` { `turn_id`, `started_at` }: turn
//!   state, not events of their own. `event_msg/task_complete` { `turn_id`,
//!   `duration_ms`, `time_to_first_token_ms`, `error?` } ends a turn;
//!   `event_msg/turn_aborted` { `reason`, `duration_ms` } is an interruption.
//! - Messages exist in up to three encodings of the same fact:
//!   `event_msg/user_message` { `message`, `images` } (0.130+) or
//!   `event_msg/item_completed` with `item.type = UserMessage` (0.135+);
//!   `event_msg/agent_message` { `message`, `phase` } or `item_completed`
//!   `AgentMessage`; and `response_item/message` with `role` user, developer
//!   or assistant (user and developer messages there are mostly injected
//!   context and are not prompts). The parser takes prompts from the first
//!   two and agent text from all three, pairing duplicates one to one.
//! - Tools, old style (every version): `response_item/function_call`
//!   { `name`, `arguments` (a JSON string), `call_id` } and
//!   `custom_tool_call` { `name`, `input` (a string), `call_id` } with their
//!   `*_output` { `call_id`, `output` } (a string, or a list of
//!   `input_text`/`input_image` parts). Tool names seen: `exec_command`,
//!   `write_stdin`, `apply_patch`, `exec` (the code-mode runner), `view_image`,
//!   `update_plan`, `spawn_agent`, `wait_agent`, `send_message`, `sleep`, ...
//! - Tools, item style (0.148+): the real work of a code-mode `exec` call is
//!   logged as `event_msg/item_completed` items with their own ids:
//!   `CommandExecution` { `command` argv, `cwd`, `exit_code`, `status`,
//!   `duration {secs,nanos}`, `aggregated_output` }, `FileChange`
//!   { `changes {path: {type, content | unified_diff}}`, `status` },
//!   `McpToolCall` { `server`, `tool`, `arguments`, `result`, `status`,
//!   `error?` }, `ImageView` { `path` }, `DynamicToolCall`, and `Extension`
//!   { `kind`: `web.search` | `image_gen.generation` | `clock.sleep` }. Those
//!   operations appear *only* here, so these items become tool-call pairs.
//! - `event_msg/token_count` { `info { total_token_usage, last_token_usage }` }
//!   after every model response: summed per turn from deltas of the
//!   running total and attached to the turn-end event as numbers.
//! - `compacted` { `message`, `replacement_history` } is a context compaction
//!   (the other two encodings, `context_compacted` and a `ContextCompaction`
//!   item, are ignored). `event_msg/thread_rolled_back` { `num_turns` } is a
//!   notification.
//! - Bookkeeping that carries no observable fact is skipped and counted:
//!   `world_state`, `token_usage_record`, `inter_agent_communication_metadata`,
//!   `realtime_item`, reasoning (private, never kept), `thread_settings_applied`,
//!   `thread_goal_updated`, the duplicate encodings above, and the
//!   `response_item` mirrors of web search and image generation. Everything
//!   else becomes a content-free `unknown` event carrying the type name, so
//!   history is never silently lost to a new Codex release.
//!
//! The parser never panics on input: malformed lines are counted, a partial
//! last line (the file is still being written) is counted separately, lines
//! over `max_line_bytes` are recognised from their first bytes and counted,
//! and unknown shapes degrade to `unknown` events.
//!
//! # Event ids
//!
//! Re-importing a file, or a file that has grown, only adds what is new, so
//! every id is a pure function of `(provider, session, key, kind)` derived in
//! ONE place, [`rollout_event_id`], which is `common::derive_event_id` for
//! Codex. `key` is the Codex `call_id` for response item tool calls
//! (`call:<id>`, spelled by `common::tool_call_key`), the item id for
//! item-style operations (`item:<id>`), otherwise the line's `ordinal`
//! (`o<n>`) or, for rollouts that predate ordinals, its line number (`l<n>`);
//! rollouts are append-only, so both are stable.
//!
//! The hook adapter derives its tool-call ids from the same function and the
//! same `call:<id>` key (Codex's hook `tool_use_id` is the model's tool call
//! id, the rollout's `call_id`), so a tool call captured by a hook and
//! imported from the rollout is one event. Whether the two ids are really the
//! same string for every Codex version is not something this repository can
//! prove from its fixtures; if they differ, nothing merges (the ids simply do
//! not collide) and the importer's check against hook-captured calls
//! (`attemptdb-capture`) is what remains. Prompts, turn ends and the like
//! have no natural id and are reconciled by the importer, not here.

use crate::CaptureContext;
use crate::common::{
    Normaliser, Payload, TOOL_OUTPUT_LIMIT, UNKNOWN_SESSION, classify_tool, derive_event_id,
    injected_prompt_kind, input_paths, is_token, to_snake, tool_call_key,
};
use attemptdb_core::event::Provider;
use attemptdb_core::{
    CaptureMode, Event, EventId, EventKind, Outcome, OutcomeStatus, ToolCategory,
};
use serde_json::{Map, Value};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::io::{self, BufRead};

/// Value of `attrs.reconstructed_from` on every event produced here.
pub const CODEX_RECONSTRUCTED_FROM: &str = "codex_rollout";

/// Prefix of every `provider_event_name`.
const NAME_PREFIX: &str = "transcript";

/// Default cap on one line, in bytes. The largest line seen in the wild is
/// 12 MB; this leaves headroom without letting a corrupt file allocate
/// without bound.
pub const DEFAULT_MAX_LINE_BYTES: usize = 32 * 1024 * 1024;

/// Bytes of an oversized line kept to recognise what it was.
const HINT_PREFIX_BYTES: usize = 4096;

/// Warnings kept per file; the rest are counted.
const MAX_WARNINGS: usize = 200;

/// `unknown` events emitted per file; the rest are counted.
const MAX_UNKNOWN_EVENTS: usize = 1000;

/// Open tool calls remembered while waiting for their output.
const MAX_PENDING_CALLS: usize = 50_000;

/// Recent messages remembered for pairing duplicate encodings.
const RECENT_WINDOW: usize = 16;

/// Longest string kept inside a tool input (bytes).
const MAX_INPUT_STRING: usize = 32 * 1024;

/// Longest prompt or agent message kept (bytes).
const MAX_TEXT: usize = 256 * 1024;

/// Longest shell command kept (bytes).
const MAX_COMMAND: usize = 64 * 1024;

/// Media ids remembered to emit one event per generated image.
const MAX_SEEN_MEDIA: usize = 4096;

/// Parser options. `include_content` normally mirrors the capture mode.
#[derive(Clone, Debug)]
pub struct CodexRolloutOptions {
    /// Keep content-bearing fields (prompts, commands, tool output, agent
    /// messages). `false` yields metadata-only events regardless of the
    /// capture mode in the context.
    pub include_content: bool,
    /// Largest tool output retained, in bytes (never more than
    /// [`TOOL_OUTPUT_LIMIT`]).
    pub max_tool_output: usize,
    /// Longest line parsed, in bytes; longer lines are recognised from
    /// their first bytes and skipped.
    pub max_line_bytes: usize,
    /// Session id to use when the file has no `session_meta` (the thread
    /// uuid at the end of the file name).
    pub session_id_hint: Option<String>,
    /// Emit `session_ended` (or `subagent_stopped`) at the end of the file.
    /// Importers turn this off for a rollout written to in the last few
    /// minutes: that session is still going.
    pub emit_session_end: bool,
}

impl Default for CodexRolloutOptions {
    fn default() -> Self {
        Self {
            include_content: true,
            max_tool_output: TOOL_OUTPUT_LIMIT,
            max_line_bytes: DEFAULT_MAX_LINE_BYTES,
            session_id_hint: None,
            emit_session_end: true,
        }
    }
}

impl CodexRolloutOptions {
    /// Options whose content policy follows a capture mode.
    pub fn for_capture_mode(mode: CaptureMode) -> Self {
        Self {
            include_content: mode.persists_content_locally(),
            ..Self::default()
        }
    }
}

/// Counts describing one parsed rollout.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct CodexRolloutStats {
    /// Non-blank lines read.
    pub lines: usize,
    /// Lines that were JSON objects (or recognisable ones too large to parse).
    pub entries: usize,
    /// Bytes read.
    pub bytes: u64,
    pub prompts: usize,
    pub messages: usize,
    pub tool_calls: usize,
    pub tool_failures: usize,
    pub turns: usize,
    pub compactions: usize,
    /// Entries of a type this parser does not know (emitted as `unknown`).
    pub unknown_entries: usize,
    /// Known-but-uninteresting entries (bookkeeping, reasoning, duplicate
    /// encodings).
    pub skipped_entries: usize,
    /// Lines of a forked subagent thread that repeat its parent's history.
    pub inherited_lines: usize,
    /// Lines that were not a JSON object.
    pub malformed_lines: usize,
    /// Lines over `max_line_bytes`.
    pub oversized_lines: usize,
    /// 1 when the file ended in the middle of a line.
    pub partial_tail: usize,
}

impl CodexRolloutStats {
    /// Lines that could not be turned into anything: malformed, oversized
    /// and the partial tail.
    pub fn lines_skipped(&self) -> usize {
        self.malformed_lines + self.oversized_lines + self.partial_tail
    }
}

/// What a parse run produced besides the events themselves.
#[derive(Clone, Debug)]
pub struct CodexRolloutSummary {
    /// The session the events were attributed to, when one was found.
    pub provider_session_id: Option<String>,
    /// Events handed to the callback.
    pub events: usize,
    pub stats: CodexRolloutStats,
    pub warnings: Vec<String>,
}

/// All events of one rollout, collected (for tests and small files; the
/// importers stream instead).
#[derive(Debug)]
pub struct CodexRolloutImport {
    pub events: Vec<Event>,
    pub summary: CodexRolloutSummary,
}

/// Content-free facts from a rollout's `session_meta` line, enough to
/// resolve the project before the file is streamed.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct RolloutMeta {
    /// The session events are attributed to: the root `session_id`, else the
    /// thread `id`. For a subagent thread this is the *parent's* session.
    pub session_id: Option<String>,
    /// This rollout's own thread id.
    pub thread_id: Option<String>,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub git_remote: Option<String>,
    pub git_commit: Option<String>,
    pub cli_version: Option<String>,
    pub originator: Option<String>,
    /// `cli`, `exec`, `vscode`, ... or `subagent`.
    pub source: Option<String>,
    pub parent_thread_id: Option<String>,
    pub agent_type: Option<String>,
    pub timestamp: Option<String>,
    pub history_start_ordinal: Option<u64>,
}

impl RolloutMeta {
    /// Whether this rollout is a subagent thread of another session.
    pub fn is_subagent(&self) -> bool {
        self.parent_thread_id.is_some()
    }

    fn from_payload(p: &Value) -> Self {
        let text = |k: &str| {
            p.get(k)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let token = |k: &str| {
            p.get(k)
                .and_then(Value::as_str)
                .filter(|s| is_token(s))
                .map(str::to_string)
        };
        let thread_id = text("id");
        let parent_thread_id = text("parent_thread_id");
        let session_id = if parent_thread_id.is_some() {
            parent_thread_id.clone().or_else(|| text("session_id"))
        } else {
            text("session_id").or_else(|| thread_id.clone())
        };
        let git = p.get("git").filter(|g| g.is_object());
        let git_text = |k: &str| {
            git.and_then(|g| g.get(k))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let (source, source_agent) = match p.get("source") {
            Some(Value::String(s)) if is_token(s) => (Some(s.clone()), None),
            Some(Value::Object(o)) => (Some("subagent".to_string()), subagent_kind(o)),
            _ => (None, None),
        };
        let agent_type = token("agent_role").or(source_agent);
        Self {
            session_id,
            thread_id,
            cwd: text("cwd"),
            git_branch: git_text("branch"),
            git_remote: git_text("repository_url"),
            git_commit: git_text("commit_hash"),
            cli_version: token("cli_version"),
            originator: p
                .get("originator")
                .and_then(Value::as_str)
                .map(to_snake)
                .filter(|s| is_token(s)),
            source,
            parent_thread_id,
            agent_type,
            timestamp: text("timestamp"),
            history_start_ordinal: p
                .get("subagent_history_start_ordinal")
                .and_then(Value::as_u64),
        }
    }
}

/// `{"subagent": {"other": "guardian"}}` or
/// `{"subagent": {"thread_spawn": {"agent_role": ...}}}`.
fn subagent_kind(source: &Map<String, Value>) -> Option<String> {
    let sub = source.get("subagent")?.as_object()?;
    if let Some(other) = sub.get("other").and_then(Value::as_str)
        && is_token(other)
    {
        return Some(other.to_string());
    }
    sub.get("thread_spawn")?
        .get("agent_role")?
        .as_str()
        .filter(|s| is_token(s))
        .map(str::to_string)
}

/// The `session_meta` facts of a rollout from its first line (`None` when
/// the line is not a `session_meta` envelope). Never reads content.
pub fn peek_rollout_meta(first_line: &[u8]) -> Option<RolloutMeta> {
    let value: Value = serde_json::from_slice(trim_ascii(first_line)).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("session_meta") {
        return None;
    }
    let payload = value.get("payload")?;
    let mut meta = RolloutMeta::from_payload(payload);
    if meta.timestamp.is_none() {
        meta.timestamp = value
            .get("timestamp")
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    Some(meta)
}

/// Derive the id of one reconstructed Codex event. The ONLY place Codex
/// transcript ids are made: `(provider, session, key, kind)`. See the module
/// documentation for the keys.
pub fn rollout_event_id(provider_session_id: &str, key: &str, kind: EventKind) -> EventId {
    derive_event_id(&Provider::Codex, provider_session_id, kind, key)
}

/// Parse one rollout, streaming events to `emit` in file order. Memory is
/// bounded by `opts.max_line_bytes` plus the open tool calls. `ctx.project`
/// is used as-is; the rollout's git branch fills `project.branch` only when
/// the context has none. An `Err` from `emit` stops the parse and is
/// returned.
pub fn parse_codex_rollout<R: BufRead, E>(
    reader: R,
    ctx: &CaptureContext,
    opts: &CodexRolloutOptions,
    mut emit: impl FnMut(Event) -> Result<(), E>,
) -> Result<CodexRolloutSummary, E> {
    let mut parser = Parser::new(ctx, opts);
    let mut lines = LineReader::new(reader, opts.max_line_bytes);
    let mut line_no = 0usize;
    let mut emitted = 0usize;
    loop {
        match lines.next_line() {
            Ok(None) => break,
            Ok(Some(info)) => {
                line_no += 1;
                parser.process_line(line_no, lines.bytes(), &info);
                for ev in parser.out.drain(..) {
                    emitted += 1;
                    emit(ev)?;
                }
            }
            Err(e) => {
                parser.warn(line_no + 1, &format!("read error, stopping here: {e}"));
                break;
            }
        }
    }
    parser.finish();
    for ev in parser.out.drain(..) {
        emitted += 1;
        emit(ev)?;
    }
    Ok(parser.summary(emitted))
}

/// [`parse_codex_rollout`] collecting every event.
pub fn parse_codex_rollout_to_vec<R: BufRead>(
    reader: R,
    ctx: &CaptureContext,
    opts: &CodexRolloutOptions,
) -> CodexRolloutImport {
    let mut events = Vec::new();
    let summary = parse_codex_rollout(reader, ctx, opts, |ev| {
        events.push(ev);
        Ok::<(), std::convert::Infallible>(())
    })
    .unwrap_or_else(|e| match e {});
    CodexRolloutImport { events, summary }
}

// ---------------------------------------------------------------------------
// Bounded line reading
// ---------------------------------------------------------------------------

/// What `next_line` found; the bytes are in [`LineReader::bytes`].
struct LineInfo {
    /// Line length in bytes, without the newline.
    total_len: u64,
    /// The line exceeded the cap: only its first bytes are in the buffer.
    oversized: bool,
    /// The line ended in a newline (false only for a final partial line).
    terminated: bool,
}

/// Reads newline-terminated lines through a buffer that never grows past
/// `cap`; the rest of an oversized line is consumed and discarded.
struct LineReader<R> {
    reader: R,
    buf: Vec<u8>,
    cap: usize,
}

impl<R: BufRead> LineReader<R> {
    fn new(reader: R, cap: usize) -> Self {
        Self {
            reader,
            buf: Vec::with_capacity(64 * 1024),
            cap: cap.max(HINT_PREFIX_BYTES),
        }
    }

    fn bytes(&self) -> &[u8] {
        &self.buf
    }

    fn next_line(&mut self) -> io::Result<Option<LineInfo>> {
        self.buf.clear();
        let mut total = 0u64;
        let mut oversized = false;
        let mut any = false;
        loop {
            let chunk = match self.reader.fill_buf() {
                Ok(c) => c,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            if chunk.is_empty() {
                return Ok(any.then_some(LineInfo {
                    total_len: total,
                    oversized,
                    terminated: false,
                }));
            }
            any = true;
            let newline = chunk.iter().position(|&b| b == b'\n');
            let body_len = newline.unwrap_or(chunk.len());
            let consume = newline.map_or(chunk.len(), |i| i + 1);
            total += body_len as u64;
            if !oversized {
                if self.buf.len() + body_len > self.cap {
                    oversized = true;
                    let keep = HINT_PREFIX_BYTES
                        .saturating_sub(self.buf.len())
                        .min(body_len);
                    self.buf.extend_from_slice(&chunk[..keep]);
                    self.buf.truncate(HINT_PREFIX_BYTES);
                } else {
                    self.buf.extend_from_slice(&chunk[..body_len]);
                }
            }
            self.reader.consume(consume);
            if newline.is_some() {
                return Ok(Some(LineInfo {
                    total_len: total,
                    oversized,
                    terminated: true,
                }));
            }
        }
    }
}

fn trim_ascii(bytes: &[u8]) -> &[u8] {
    let start = bytes
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(start, |i| i + 1);
    &bytes[start..end]
}

// ---------------------------------------------------------------------------
// Prefix hints
// ---------------------------------------------------------------------------

/// What the first bytes of an envelope line say, without parsing it. The
/// envelope key order is stable (`timestamp`, [`ordinal`,] `type`,
/// `payload{type, call_id?, ...}`), so a sequential scan finds the type of a
/// line whose body is too large to parse, and lets bookkeeping lines be
/// skipped without building a JSON tree for them.
#[derive(Clone, Copy, Debug, Default)]
struct Hint<'a> {
    ts: Option<&'a str>,
    ordinal: Option<u64>,
    top: Option<&'a str>,
    sub: Option<&'a str>,
    call_id: Option<&'a str>,
}

impl<'a> Hint<'a> {
    fn of(line: &'a [u8]) -> Hint<'a> {
        let mut hint = Hint::default();
        let Some(mut rest) = line.strip_prefix(br#"{"timestamp":""#) else {
            return hint;
        };
        let Some((ts, after)) = token_until_quote(rest) else {
            return hint;
        };
        hint.ts = Some(ts);
        rest = after;
        if let Some(r) = rest.strip_prefix(br#","ordinal":"#) {
            let digits = r.iter().take_while(|b| b.is_ascii_digit()).count();
            hint.ordinal = std::str::from_utf8(&r[..digits])
                .ok()
                .and_then(|d| d.parse().ok());
            rest = &r[digits..];
        }
        let Some(r) = rest.strip_prefix(br#","type":""#) else {
            return hint;
        };
        let Some((top, after)) = token_until_quote(r) else {
            return hint;
        };
        hint.top = Some(top);
        rest = after;
        let Some(r) = rest.strip_prefix(br#","payload":{"type":""#) else {
            return hint;
        };
        let Some((sub, after)) = token_until_quote(r) else {
            return hint;
        };
        hint.sub = Some(sub);
        if let Some(r) = after.strip_prefix(br#","call_id":""#)
            && let Some((id, _)) = token_until_quote(r)
        {
            hint.call_id = Some(id);
        }
        hint
    }

    /// Lines that carry no fact and are skipped without parsing.
    fn is_bookkeeping(&self) -> bool {
        matches!(
            (self.top, self.sub),
            (
                Some(
                    "world_state"
                        | "token_usage_record"
                        | "inter_agent_communication_metadata"
                        | "realtime_item"
                ),
                _
            ) | (
                Some("response_item"),
                Some("reasoning" | "image_generation_call" | "web_search_call")
            ) | (
                Some("event_msg"),
                Some(
                    "thread_settings_applied"
                        | "thread_goal_updated"
                        | "context_compacted"
                        | "patch_apply_end"
                )
            )
        )
    }
}

/// A short ASCII-ish token up to the next `"`, and what follows the quote.
/// Refuses anything with an escape or a very long value: a hint is only
/// trusted for plain tokens.
fn token_until_quote(bytes: &[u8]) -> Option<(&str, &[u8])> {
    let end = bytes.iter().take(256).position(|&b| b == b'"')?;
    let token = std::str::from_utf8(&bytes[..end]).ok()?;
    if token.contains('\\') {
        return None;
    }
    Some((token, &bytes[end + 1..]))
}

// ---------------------------------------------------------------------------
// Parser state
// ---------------------------------------------------------------------------

/// Where a message was seen; duplicates pair across different encodings.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Encoding {
    EventMsg,
    Item,
    ResponseItem,
}

struct Recent {
    turn: Option<String>,
    hash: u64,
    encoding: Encoding,
}

/// Token counters as Codex reports them.
#[derive(Clone, Copy, Default)]
struct Usage {
    input: u64,
    cached_input: u64,
    output: u64,
    reasoning: u64,
    total: u64,
}

impl Usage {
    fn of(v: &Value) -> Option<Self> {
        let n = |k: &str| v.get(k).and_then(Value::as_u64).unwrap_or(0);
        v.is_object().then(|| Self {
            input: n("input_tokens"),
            cached_input: n("cached_input_tokens"),
            output: n("output_tokens"),
            reasoning: n("reasoning_output_tokens"),
            total: n("total_tokens"),
        })
    }

    fn since(self, base: Option<Usage>) -> Usage {
        let b = base.unwrap_or_default();
        Usage {
            input: self.input.saturating_sub(b.input),
            cached_input: self.cached_input.saturating_sub(b.cached_input),
            output: self.output.saturating_sub(b.output),
            reasoning: self.reasoning.saturating_sub(b.reasoning),
            total: self.total.saturating_sub(b.total),
        }
    }
}

/// What is remembered about a tool call that has started but not finished.
struct PendingCall {
    name: String,
    facts: InputFacts,
    started_ms: Option<i64>,
}

/// Content-free (plus the command text) facts derived from a tool input,
/// computed once so the start and the finish event agree.
#[derive(Clone, Default)]
struct InputFacts {
    paths: Vec<String>,
    command: Option<String>,
    delta: Option<(u64, u64)>,
}

impl InputFacts {
    fn of(name: &str, input: &Value) -> Self {
        let mut facts = Self::default();
        let Some(map) = input.as_object() else {
            return facts;
        };
        if let Some(cmd) = map.get("cmd").or_else(|| map.get("command")) {
            facts.command = script_of(cmd).map(|s| bound_text(&s, MAX_COMMAND).0);
        }
        // The code-mode runner's input is source code that merely mentions
        // tools: its paths are not paths the call touched.
        if name != "exec" {
            facts.paths = input_paths(map);
        }
        if name == "apply_patch"
            && let Some(patch) = map.get("input").and_then(Value::as_str)
        {
            facts.delta = Some(patch_line_delta(patch));
        }
        facts
    }

    fn apply(&self, n: &mut Normaliser<'_>) {
        for p in &self.paths {
            n.add_file(p);
        }
        if let Some(c) = &self.command {
            n.set_command(c);
        }
        if let Some((added, removed)) = self.delta {
            n.set_edit_delta(added, removed);
        }
    }
}

/// How a tool call ended.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Ending {
    Success,
    /// Returned while the process or script is still running.
    Running,
    Failure,
    Cancelled,
    Denied,
    Unknown,
}

/// What the output of a tool call says about how it went.
struct OutputFacts {
    text: String,
    images: usize,
    ending: Ending,
    exit_code: Option<i32>,
    wall_ms: Option<u64>,
}

struct Parser<'a> {
    ctx: &'a CaptureContext,
    opts: &'a CodexRolloutOptions,
    out: Vec<Event>,
    stats: CodexRolloutStats,
    warnings: Vec<String>,
    suppressed_warnings: usize,
    // identity
    session: Option<String>,
    meta: Option<RolloutMeta>,
    meta_seen: bool,
    started: bool,
    // running state
    cwd: Option<String>,
    model: Option<String>,
    permission_mode: Option<String>,
    turn_id: Option<String>,
    turn_index: u64,
    last_ts: Option<String>,
    last_pos: String,
    // pairing and duplicate handling
    tools: HashMap<String, PendingCall>,
    pending_overflow_warned: bool,
    seen_media: HashSet<String>,
    recent_prompts: VecDeque<Recent>,
    recent_messages: VecDeque<Recent>,
    unknown_emitted: usize,
    // token accounting
    total_usage: Option<Usage>,
    turn_base: Option<Usage>,
    context_window: Option<u64>,
}

impl<'a> Parser<'a> {
    fn new(ctx: &'a CaptureContext, opts: &'a CodexRolloutOptions) -> Self {
        Self {
            ctx,
            opts,
            out: Vec::new(),
            stats: CodexRolloutStats::default(),
            warnings: Vec::new(),
            suppressed_warnings: 0,
            session: None,
            meta: None,
            meta_seen: false,
            started: false,
            cwd: None,
            model: None,
            permission_mode: None,
            turn_id: None,
            turn_index: 0,
            last_ts: None,
            last_pos: "l0".to_string(),
            tools: HashMap::new(),
            pending_overflow_warned: false,
            seen_media: HashSet::new(),
            recent_prompts: VecDeque::new(),
            recent_messages: VecDeque::new(),
            unknown_emitted: 0,
            total_usage: None,
            turn_base: None,
            context_window: None,
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

    fn summary(mut self, events: usize) -> CodexRolloutSummary {
        if self.suppressed_warnings > 0 {
            self.warnings.push(format!(
                "{} further warning(s) suppressed",
                self.suppressed_warnings
            ));
        }
        CodexRolloutSummary {
            provider_session_id: self.session.filter(|s| s != UNKNOWN_SESSION),
            events,
            stats: self.stats,
            warnings: self.warnings,
        }
    }

    // --- identity -----------------------------------------------------------

    fn session(&self) -> String {
        self.session
            .clone()
            .unwrap_or_else(|| UNKNOWN_SESSION.to_string())
    }

    fn is_subagent(&self) -> bool {
        self.meta.as_ref().is_some_and(RolloutMeta::is_subagent)
    }

    fn history_start(&self) -> Option<u64> {
        self.meta.as_ref().and_then(|m| m.history_start_ordinal)
    }

    /// Decide the session when the file has no `session_meta` (a truncated
    /// head): the hint from the file name, else the unknown session.
    fn fallback_session(&mut self, line_no: usize) {
        if self.session.is_some() {
            return;
        }
        match &self.opts.session_id_hint {
            Some(hint) => {
                self.warn(
                    line_no,
                    &format!("no session_meta first; using the file name's session {hint:?}"),
                );
                self.session = Some(hint.clone());
            }
            None => {
                self.warn(
                    line_no,
                    &format!(
                        "no session_meta first and no file name hint; events attributed to session {UNKNOWN_SESSION:?}"
                    ),
                );
                self.session = Some(UNKNOWN_SESSION.to_string());
            }
        }
    }

    // --- line dispatch ------------------------------------------------------

    fn process_line(&mut self, line_no: usize, raw: &[u8], info: &LineInfo) {
        let line = trim_ascii(raw);
        if line.is_empty() && !info.oversized {
            return;
        }
        self.stats.lines += 1;
        self.stats.bytes += info.total_len + u64::from(info.terminated);
        let hint = Hint::of(line);
        if let Some(ts) = hint
            .ts
            .filter(|t| attemptdb_core::Timestamp::parse(t).is_some())
        {
            self.last_ts = Some(ts.to_string());
        }
        if let (Some(start), Some(ordinal)) = (self.history_start(), hint.ordinal)
            && ordinal < start
        {
            self.stats.inherited_lines += 1;
            return;
        }
        if hint.is_bookkeeping() {
            // Recognised from its first bytes, however large it is.
            self.stats.entries += 1;
            self.stats.skipped_entries += 1;
            return;
        }
        if info.oversized {
            self.oversized(line_no, &hint, info);
            return;
        }
        let value = match parse_value(line) {
            Some(v) => v,
            None => {
                if info.terminated {
                    self.stats.malformed_lines += 1;
                    self.warn(line_no, "invalid JSON");
                } else {
                    self.stats.partial_tail += 1;
                    self.warn(line_no, "the file ends in the middle of a line (still being written?); that line is ignored");
                }
                return;
            }
        };
        if !value.is_object() {
            self.stats.malformed_lines += 1;
            self.warn(line_no, "not a JSON object");
            return;
        }
        self.stats.entries += 1;
        if hint.ts.is_none()
            && let Some(ts) = value
                .get("timestamp")
                .and_then(Value::as_str)
                .filter(|t| attemptdb_core::Timestamp::parse(t).is_some())
        {
            self.last_ts = Some(ts.to_string());
        }
        let ordinal = value.get("ordinal").and_then(Value::as_u64);
        if let (Some(start), Some(o)) = (self.history_start(), ordinal)
            && o < start
        {
            self.stats.inherited_lines += 1;
            return;
        }
        let pos = match ordinal {
            Some(o) => format!("o{o}"),
            None => format!("l{line_no}"),
        };
        self.last_pos.clone_from(&pos);
        self.dispatch(line_no, &pos, &value);
    }

    fn dispatch(&mut self, line_no: usize, pos: &str, v: &Value) {
        let top = v.get("type").and_then(Value::as_str).unwrap_or("");
        if top.is_empty() {
            self.warn(line_no, "entry has no `type`");
            self.ensure_started(line_no);
            self.unknown("untyped", pos);
            return;
        }
        // Rollouts from before the envelope carried response items bare:
        // `{"type":"message","role":...}`.
        let (top, payload) = match v.get("payload") {
            Some(p) => (top, p),
            None if is_response_item_type(top) => ("response_item", v),
            None => (top, v),
        };
        if top == "session_meta" {
            self.session_meta(line_no, payload);
            return;
        }
        self.ensure_started(line_no);
        let sub = payload.get("type").and_then(Value::as_str).unwrap_or("");
        match top {
            "turn_context" => self.turn_context(payload),
            "event_msg" => self.event_msg(line_no, pos, sub, payload),
            "response_item" => self.response_item(pos, sub, payload),
            "compacted" => self.compacted(pos, payload),
            "world_state"
            | "token_usage_record"
            | "inter_agent_communication_metadata"
            | "realtime_item" => self.stats.skipped_entries += 1,
            other => {
                let tag = other.to_string();
                self.unknown(&tag, pos);
            }
        }
    }

    // --- session ------------------------------------------------------------

    fn session_meta(&mut self, line_no: usize, payload: &Value) {
        if self.meta_seen {
            // A forked thread embeds its parent's meta line; a resumed one
            // may repeat it. The first one defines the session.
            self.stats.skipped_entries += 1;
            return;
        }
        self.meta_seen = true;
        let meta = RolloutMeta::from_payload(payload);
        match &meta.session_id {
            Some(s) => self.session = Some(s.clone()),
            None => self.fallback_session(line_no),
        }
        self.cwd.clone_from(&meta.cwd);
        self.meta = Some(meta);
        self.ensure_started(line_no);
    }

    fn ensure_started(&mut self, line_no: usize) {
        if self.started {
            return;
        }
        if self.session.is_none() {
            self.fallback_session(line_no);
        }
        self.started = true;
        let meta = self.meta.clone().unwrap_or_default();
        let name = "session_meta";
        if meta.is_subagent() {
            self.emit_event(
                &format!("{name}:subagent"),
                EventKind::SubagentStarted,
                "start",
                None,
                move |n| {
                    n.attr("source", "transcript");
                    if let Some(parent) = &meta.parent_thread_id {
                        n.provider_attr("parent_thread_id", parent.as_str());
                    }
                    session_attrs(n, &meta);
                },
            );
        } else {
            self.emit_event(name, EventKind::SessionStarted, "session", None, move |n| {
                n.attr("source", "transcript");
                session_attrs(n, &meta);
            });
        }
    }

    // --- event construction -------------------------------------------------

    /// Build one event. Every reconstructed event goes through here so the
    /// provenance attributes, id derivation and content policy are applied
    /// uniformly. `ts_ms` overrides the line's timestamp (item-style
    /// operations carry their own start and end times).
    fn emit_event(
        &mut self,
        tag: &str,
        kind: EventKind,
        key: &str,
        ts_ms: Option<i64>,
        fill: impl FnOnce(&mut Normaliser<'_>),
    ) {
        let session = self.session();
        let mut synth = Map::new();
        if let Some(cwd) = &self.cwd {
            synth.insert("cwd".into(), Value::String(cwd.clone()));
        }
        match (ts_ms, &self.last_ts) {
            (Some(ms), _) => {
                synth.insert("timestamp".into(), Value::from(ms));
            }
            (None, Some(ts)) => {
                synth.insert("timestamp".into(), Value::String(ts.clone()));
            }
            _ => {}
        }
        let synth = Value::Object(synth);
        let payload = Payload::from_value(&synth).expect("synthetic payload is an object");
        let mut n = Normaliser::new(
            self.ctx,
            payload,
            Provider::Codex,
            &format!("{NAME_PREFIX}:{tag}"),
            kind,
            &session,
        );
        n.set_cwd();
        n.attr("reconstructed", true);
        n.attr("reconstructed_from", CODEX_RECONSTRUCTED_FROM);
        n.attr("transcript_entry_type", tag);
        n.attr("transcript_present", true);
        let subagent = self.meta.as_ref().filter(|m| m.is_subagent());
        match subagent {
            Some(m) => {
                let thread = m.thread_id.clone().unwrap_or_else(|| "unknown".into());
                n.set_subagent(&thread, m.agent_type.as_deref());
                n.attr("is_sidechain", true);
            }
            None => n.attr("turn_index_hint", self.turn_index),
        }
        fill(&mut n);
        let mut ev = n.finish();
        ev.raw = None;
        ev.hook_version = None;
        ev.provider_version = self.meta.as_ref().and_then(|m| m.cli_version.clone());
        if ev.project.branch.is_none() {
            ev.project.branch = self.meta.as_ref().and_then(|m| m.git_branch.clone());
        }
        ev.provider_turn_id.clone_from(&self.turn_id);
        if ev.agent.model.is_none() {
            ev.agent.model.clone_from(&self.model);
        }
        ev.event_id = rollout_event_id(&session, key, kind);
        ev.attrs.remove("hook_event_name");
        if !self.opts.include_content {
            ev.content = None;
        }
        self.out.push(ev);
    }

    fn unknown(&mut self, tag: &str, pos: &str) {
        self.stats.unknown_entries += 1;
        if self.unknown_emitted >= MAX_UNKNOWN_EVENTS {
            return;
        }
        self.unknown_emitted += 1;
        self.emit_event(tag, EventKind::Unknown, pos, None, |_| {});
    }

    // --- turn state ---------------------------------------------------------

    fn turn_context(&mut self, p: &Value) {
        if let Some(cwd) = p
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            self.cwd = Some(cwd.to_string());
        }
        if let Some(model) = p
            .get("model")
            .and_then(Value::as_str)
            .filter(|s| is_token(s))
        {
            self.model = Some(model.to_string());
        }
        if let Some(mode) = p
            .get("approval_policy")
            .and_then(Value::as_str)
            .filter(|s| is_token(s))
        {
            self.permission_mode = Some(mode.to_string());
        }
        if let Some(id) = p
            .get("turn_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            self.turn_id = Some(id.to_string());
        }
        self.stats.skipped_entries += 1;
    }

    fn task_started(&mut self, p: &Value) {
        self.turn_id = p
            .get("turn_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        self.turn_index += 1;
        self.turn_base = self.total_usage;
        if let Some(w) = p.get("model_context_window").and_then(Value::as_u64) {
            self.context_window = Some(w);
        }
        self.stats.skipped_entries += 1;
    }

    fn token_count(&mut self, p: &Value) {
        if let Some(info) = p.get("info").filter(|i| i.is_object()) {
            if let Some(u) = info.get("total_token_usage").and_then(Usage::of) {
                self.total_usage = Some(u);
            }
            if let Some(w) = info.get("model_context_window").and_then(Value::as_u64) {
                self.context_window = Some(w);
            }
        }
        self.stats.skipped_entries += 1;
    }

    /// Token counters of the turn that just ended, attached as numbers.
    fn turn_usage(&self) -> Option<Usage> {
        let total = self.total_usage?;
        let used = total.since(self.turn_base);
        (used.total > 0).then_some(used)
    }

    fn turn_end(&mut self, pos: &str, p: &Value, aborted: bool) {
        self.stats.turns += 1;
        let duration = p.get("duration_ms").and_then(Value::as_u64);
        let ttft = p.get("time_to_first_token_ms").and_then(Value::as_u64);
        let usage = self.turn_usage();
        let window = self.context_window;
        let error = p.get("error").filter(|e| !e.is_null());
        let error_message = error.and_then(|e| {
            e.get("message")
                .and_then(Value::as_str)
                .or_else(|| e.as_str())
                .map(|s| bound_text(s, MAX_TEXT).0)
        });
        let error_class = error.and_then(error_class_of);
        let reason = p
            .get("reason")
            .and_then(Value::as_str)
            .filter(|s| is_token(s))
            .map(to_snake);
        let (tag, kind) = if aborted {
            ("event_msg:turn_aborted", EventKind::TurnFailed)
        } else if error.is_some() {
            ("event_msg:task_complete", EventKind::TurnFailed)
        } else {
            ("event_msg:task_complete", EventKind::TurnStopped)
        };
        let key = format!("turn:{}:{pos}", self.turn_id.as_deref().unwrap_or("-"));
        self.emit_event(tag, kind, &key, None, move |n| {
            n.event.duration_ms = duration;
            if let Some(t) = ttft {
                n.provider_attr("time_to_first_token_ms", t);
            }
            if let Some(u) = usage {
                n.attr("output_tokens", u.output);
                n.provider_attr("input_tokens", u.input);
                n.provider_attr("cached_input_tokens", u.cached_input);
                n.provider_attr("reasoning_output_tokens", u.reasoning);
                n.provider_attr("total_tokens", u.total);
            }
            if let Some(w) = window {
                n.provider_attr("context_window", w);
            }
            if aborted {
                n.attr("reason", reason.unwrap_or_else(|| "user_interrupt".into()));
                n.attr("error_class", "interrupted");
                n.event.outcome = Some(Outcome {
                    status: OutcomeStatus::Cancelled,
                    class: Some("interrupted".to_string()),
                    exit_code: None,
                });
            } else if error.is_some() {
                match &error_class {
                    Some(c) => n.set_failure_with_class(c, error_message.as_deref()),
                    None => n.set_failure(error_message.as_deref(), None),
                }
            }
        });
        self.turn_base = self.total_usage;
    }

    // --- event_msg ----------------------------------------------------------

    fn event_msg(&mut self, line_no: usize, pos: &str, sub: &str, p: &Value) {
        match sub {
            "task_started" => self.task_started(p),
            "task_complete" => self.turn_end(pos, p, false),
            "turn_aborted" => self.turn_end(pos, p, true),
            "token_count" => self.token_count(p),
            "user_message" => {
                let text = p
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let images = count_list(p.get("images")) + count_list(p.get("local_images"));
                self.prompt(pos, text, images, Encoding::EventMsg);
            }
            "agent_message" => {
                let text = p
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let phase = token_field(p, "phase");
                self.agent_message(pos, text, phase, Encoding::EventMsg);
            }
            "item_completed" => self.item_completed(pos, p),
            "web_search_end" => {
                let call_id = str_field(p, "call_id");
                let query = str_field(p, "query")
                    .or_else(|| p.get("action").and_then(|a| str_field(a, "query")));
                self.web_search(pos, call_id, query);
            }
            "image_generation_end" => {
                let id = str_field(p, "call_id");
                self.image_generation(pos, id, p, None);
            }
            "mcp_tool_call_end" => self.mcp_tool_call_end(pos, p),
            "thread_rolled_back" => {
                let turns = p.get("num_turns").and_then(Value::as_u64);
                self.emit_event(
                    "event_msg:thread_rolled_back",
                    EventKind::Notification,
                    pos,
                    None,
                    move |n| {
                        n.attr("notification_type", "thread_rolled_back");
                        if let Some(t) = turns {
                            n.provider_attr("rolled_back_turns", t);
                        }
                    },
                );
            }
            "thread_settings_applied"
            | "thread_goal_updated"
            | "context_compacted"
            | "patch_apply_end" => self.stats.skipped_entries += 1,
            "" => {
                self.warn(line_no, "event_msg without a payload type");
                self.unknown("event_msg", pos);
            }
            other => {
                let tag = format!("event_msg:{}", tag_token(other));
                self.unknown(&tag, pos);
            }
        }
    }

    // --- prompts and messages -----------------------------------------------

    /// Pair a message with its duplicate in another encoding: true when
    /// `text` was already seen (one to one) in this turn.
    fn is_duplicate(
        recent: &mut VecDeque<Recent>,
        turn: &Option<String>,
        text: &str,
        encoding: Encoding,
    ) -> bool {
        let mut h = DefaultHasher::new();
        text.hash(&mut h);
        let hash = h.finish();
        if let Some(i) = recent
            .iter()
            .position(|r| r.hash == hash && &r.turn == turn && r.encoding != encoding)
        {
            recent.remove(i);
            return true;
        }
        recent.push_back(Recent {
            turn: turn.clone(),
            hash,
            encoding,
        });
        if recent.len() > RECENT_WINDOW {
            recent.pop_front();
        }
        false
    }

    fn prompt(&mut self, pos: &str, text: String, images: usize, encoding: Encoding) {
        if Self::is_duplicate(&mut self.recent_prompts, &self.turn_id, &text, encoding) {
            self.stats.skipped_entries += 1;
            return;
        }
        self.stats.prompts += 1;
        let kind = injected_prompt_kind(&text).unwrap_or("text");
        let full_chars = text.chars().count() as u64;
        let (bounded, truncated) = bound_text(&text, MAX_TEXT);
        let mode = self.permission_mode.clone();
        self.emit_event(
            &format!("{}:user_message", encoding.tag()),
            EventKind::PromptSubmitted,
            pos,
            None,
            move |n| {
                n.set_prompt(&bounded);
                if truncated {
                    n.attr("prompt_chars", full_chars);
                    n.provider_attr("prompt_truncated", true);
                }
                n.provider_attr("prompt_kind", kind);
                if let Some(m) = mode {
                    n.attr("permission_mode", m);
                }
                if images > 0 {
                    n.attr("image_count", images as u64);
                }
            },
        );
    }

    fn agent_message(&mut self, pos: &str, text: String, phase: Option<String>, e: Encoding) {
        if text.trim().is_empty() {
            self.stats.skipped_entries += 1;
            return;
        }
        if Self::is_duplicate(&mut self.recent_messages, &self.turn_id, &text, e) {
            self.stats.skipped_entries += 1;
            return;
        }
        self.stats.messages += 1;
        let chars = text.chars().count() as u64;
        let (bounded, truncated) = bound_text(&text, MAX_TEXT);
        self.emit_event(
            &format!("{}:agent_message", e.tag()),
            EventKind::AgentMessage,
            pos,
            None,
            move |n| {
                n.set_message(&bounded);
                n.provider_attr("message_chars", chars);
                if truncated {
                    n.provider_attr("message_truncated", true);
                }
                if let Some(p) = phase {
                    n.provider_attr("phase", p);
                }
            },
        );
    }

    // --- response items -----------------------------------------------------

    fn response_item(&mut self, pos: &str, sub: &str, p: &Value) {
        match sub {
            "message" => match p.get("role").and_then(Value::as_str) {
                Some("assistant") => {
                    let text = content_text(p.get("content"));
                    self.agent_message(pos, text, None, Encoding::ResponseItem);
                }
                // Injected context (instructions, environment) and the
                // user's own text, which `user_message` carries.
                _ => self.stats.skipped_entries += 1,
            },
            "function_call" | "custom_tool_call" | "local_shell_call" => {
                self.response_tool_call(pos, sub, p)
            }
            "function_call_output" | "custom_tool_call_output" | "local_shell_call_output" => {
                self.response_tool_output(pos, sub, p)
            }
            "reasoning" | "web_search_call" | "image_generation_call" | "agent_message" => {
                self.stats.skipped_entries += 1
            }
            "" => self.unknown("response_item", pos),
            other => {
                let tag = format!("response_item:{}", tag_token(other));
                self.unknown(&tag, pos);
            }
        }
    }

    fn response_tool_call(&mut self, pos: &str, sub: &str, p: &Value) {
        let name = str_field(p, "name").unwrap_or_else(|| {
            if sub == "local_shell_call" {
                "local_shell".to_string()
            } else {
                "unknown".to_string()
            }
        });
        let call_id = str_field(p, "call_id").or_else(|| str_field(p, "id"));
        let input = match sub {
            "function_call" => match p.get("arguments") {
                Some(Value::String(s)) => serde_json::from_str::<Value>(s)
                    .ok()
                    .filter(Value::is_object)
                    .unwrap_or_else(|| serde_json::json!({ "arguments": s })),
                Some(v) => v.clone(),
                None => Value::Object(Map::new()),
            },
            "local_shell_call" => p
                .get("action")
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new())),
            _ => match p.get("input") {
                Some(Value::String(s)) => serde_json::json!({ "input": s }),
                Some(v) => v.clone(),
                None => Value::Object(Map::new()),
            },
        };
        let key = call_key(call_id.as_deref(), pos);
        self.tool_started(
            &format!("response_item:{sub}"),
            &key,
            None,
            &name,
            call_id,
            input,
        );
    }

    fn response_tool_output(&mut self, pos: &str, sub: &str, p: &Value) {
        let call_id = str_field(p, "call_id").or_else(|| str_field(p, "id"));
        let (name, facts, started_ms) = match call_id.as_ref().and_then(|c| self.tools.remove(c)) {
            Some(pending) => (pending.name, pending.facts, pending.started_ms),
            None => ("unknown".to_string(), InputFacts::default(), None),
        };
        let output = p.get("output").cloned().unwrap_or(Value::Null);
        let facts_out = interpret_output(&name, &output);
        let key = call_key(call_id.as_deref(), pos);
        let duration = facts_out.wall_ms.or_else(|| self.elapsed_since(started_ms));
        self.tool_finished(
            &format!("response_item:{sub}"),
            &key,
            None,
            &name,
            call_id,
            &facts,
            facts_out,
            duration,
        );
    }

    fn elapsed_since(&self, started_ms: Option<i64>) -> Option<u64> {
        let start = started_ms?;
        let now = self
            .last_ts
            .as_deref()
            .and_then(attemptdb_core::Timestamp::parse)?
            .as_micros()
            / 1000;
        u64::try_from(now - start).ok()
    }

    // --- tool pairs ---------------------------------------------------------

    /// Emit a `tool_call_started` and remember the call until its output.
    fn tool_started(
        &mut self,
        tag: &str,
        key: &str,
        ts_ms: Option<i64>,
        name: &str,
        call_id: Option<String>,
        input: Value,
    ) {
        self.stats.tool_calls += 1;
        let facts = InputFacts::of(name, &input);
        let (bounded, truncated) = bound_input(&input, MAX_INPUT_STRING);
        let started_ms = ts_ms.or_else(|| {
            self.last_ts
                .as_deref()
                .and_then(attemptdb_core::Timestamp::parse)
                .map(|t| t.as_micros() / 1000)
        });
        if let Some(id) = &call_id {
            if self.tools.len() >= MAX_PENDING_CALLS {
                if !self.pending_overflow_warned {
                    self.pending_overflow_warned = true;
                    self.warnings.push(format!(
                        "more than {MAX_PENDING_CALLS} tool calls without an output; forgetting the open ones"
                    ));
                }
                self.tools.clear();
            }
            self.tools.insert(
                id.clone(),
                PendingCall {
                    name: name.to_string(),
                    facts: facts.clone(),
                    started_ms,
                },
            );
        }
        let name_owned = name.to_string();
        self.emit_event(tag, EventKind::ToolCallStarted, key, ts_ms, move |n| {
            n.set_tool(&name_owned, call_id.as_deref());
            if let Some(t) = n.event.tool.as_mut() {
                t.category = codex_category(&name_owned);
            }
            n.set_tool_input(&bounded);
            if truncated {
                n.provider_attr("input_truncated", true);
            }
            facts.apply(n);
        });
    }

    /// Emit the end of a call: `tool_call_finished`, or `tool_call_failed`
    /// when it failed, was cancelled or was declined.
    #[allow(clippy::too_many_arguments)]
    fn tool_finished(
        &mut self,
        tag: &str,
        key: &str,
        ts_ms: Option<i64>,
        name: &str,
        call_id: Option<String>,
        facts: &InputFacts,
        out: OutputFacts,
        duration_ms: Option<u64>,
    ) {
        let failed = matches!(
            out.ending,
            Ending::Failure | Ending::Cancelled | Ending::Denied
        );
        if failed {
            self.stats.tool_failures += 1;
        }
        let kind = if failed {
            EventKind::ToolCallFailed
        } else {
            EventKind::ToolCallFinished
        };
        let max = self.opts.max_tool_output.min(TOOL_OUTPUT_LIMIT);
        let (bounded, truncated) = bound_text(&out.text, max);
        let name_owned = name.to_string();
        let facts = facts.clone();
        self.emit_event(tag, kind, key, ts_ms, move |n| {
            n.set_tool(&name_owned, call_id.as_deref());
            if let Some(t) = n.event.tool.as_mut() {
                t.category = codex_category(&name_owned);
            }
            facts.apply(n);
            n.event.duration_ms = duration_ms;
            if out.images > 0 {
                n.attr("image_count", out.images as u64);
            }
            if !bounded.is_empty() {
                if truncated {
                    n.attr("tool_output_truncated", true);
                }
                n.set_tool_output(&Value::String(bounded.clone()));
            }
            let error_text = (!bounded.is_empty()).then_some(bounded.as_str());
            match out.ending {
                Ending::Success => n.set_success(out.exit_code),
                Ending::Running => {
                    n.set_success(out.exit_code);
                    n.provider_attr("still_running", true);
                }
                Ending::Failure => n.set_failure(error_text, out.exit_code),
                Ending::Cancelled => {
                    n.set_failure_with_class("interrupted", None);
                    if let Some(o) = n.event.outcome.as_mut() {
                        o.status = OutcomeStatus::Cancelled;
                    }
                }
                Ending::Denied => {
                    n.set_failure_with_class("user_rejected", None);
                    if let Some(o) = n.event.outcome.as_mut() {
                        o.status = OutcomeStatus::Denied;
                    }
                }
                Ending::Unknown => {
                    n.event.outcome = Some(Outcome {
                        status: OutcomeStatus::Unknown,
                        class: None,
                        exit_code: out.exit_code,
                    });
                }
            }
        });
    }

    /// A whole operation that the rollout logs only once, when it is done:
    /// emit its start and its end.
    #[allow(clippy::too_many_arguments)]
    fn tool_pair(
        &mut self,
        tag: &str,
        key: &str,
        started_ms: Option<i64>,
        ended_ms: Option<i64>,
        name: &str,
        call_id: Option<String>,
        input: Value,
        out: OutputFacts,
        duration_ms: Option<u64>,
    ) {
        let facts = InputFacts::of(name, &input);
        self.tool_started(tag, key, started_ms, name, call_id.clone(), input);
        if let Some(id) = &call_id {
            self.tools.remove(id);
        }
        self.tool_finished(tag, key, ended_ms, name, call_id, &facts, out, duration_ms);
    }

    // --- item_completed -----------------------------------------------------

    fn item_completed(&mut self, pos: &str, p: &Value) {
        let Some(item) = p.get("item").filter(|i| i.is_object()) else {
            self.unknown("event_msg:item_completed", pos);
            return;
        };
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
        let id = str_field(item, "id");
        let key = id
            .as_deref()
            .map(|i| format!("item:{i}"))
            .unwrap_or_else(|| pos.to_string());
        let started = p.get("started_at_ms").and_then(Value::as_i64);
        let ended = p.get("completed_at_ms").and_then(Value::as_i64);
        let tag = format!("event_msg:item_completed:{}", tag_token(item_type));
        match item_type {
            "UserMessage" => {
                let text = content_text(item.get("content"));
                self.prompt(pos, text, 0, Encoding::Item);
            }
            "AgentMessage" => {
                let text = content_text(item.get("content"));
                let phase = token_field(item, "phase");
                self.agent_message(pos, text, phase, Encoding::Item);
            }
            "CommandExecution" => self.command_execution(&tag, &key, id, started, ended, item),
            "FileChange" => self.file_change(&tag, &key, id, started, ended, item),
            "McpToolCall" => self.mcp_item(&tag, &key, id, started, ended, item),
            "DynamicToolCall" => self.dynamic_tool(&tag, &key, id, started, ended, item),
            "ImageView" => {
                let input = serde_json::json!({
                    "path": item.get("path").and_then(Value::as_str).unwrap_or_default()
                });
                let ok = ok_output();
                self.tool_pair(
                    &tag,
                    &key,
                    started,
                    ended,
                    "view_image",
                    id,
                    input,
                    ok,
                    None,
                );
            }
            "Extension" => self.extension(pos, &tag, id, started, ended, item),
            // The same facts are logged elsewhere (reasoning is private;
            // the others mirror response-item tool calls and `compacted`).
            "Reasoning"
            | "SubAgentActivity"
            | "CollabAgentToolCall"
            | "FunctionCallOutput"
            | "ContextCompaction" => self.stats.skipped_entries += 1,
            "" => self.unknown("event_msg:item_completed", pos),
            _ => self.unknown(&tag, pos),
        }
    }

    fn command_execution(
        &mut self,
        tag: &str,
        key: &str,
        id: Option<String>,
        started: Option<i64>,
        ended: Option<i64>,
        item: &Value,
    ) {
        let script = item.get("command").and_then(script_of);
        let mut input = Map::new();
        if let Some(s) = &script {
            input.insert("cmd".into(), Value::String(s.clone()));
        }
        if let Some(cwd) = str_field(item, "cwd") {
            input.insert("workdir".into(), Value::String(cwd));
        }
        let exit_code = item
            .get("exit_code")
            .and_then(Value::as_i64)
            .and_then(|c| i32::try_from(c).ok());
        let status = item.get("status").and_then(Value::as_str).unwrap_or("");
        let ending = match (status, exit_code) {
            ("declined", _) => Ending::Denied,
            ("failed", _) => Ending::Failure,
            (_, Some(c)) if c != 0 => Ending::Failure,
            ("completed", _) | (_, Some(0)) => Ending::Success,
            _ => Ending::Unknown,
        };
        let text = ["aggregated_output", "formatted_output", "stdout"]
            .iter()
            .find_map(|k| {
                item.get(*k)
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_default()
            .to_string();
        let out = OutputFacts {
            text,
            images: 0,
            ending,
            exit_code,
            wall_ms: None,
        };
        let duration = duration_ms(item.get("duration"));
        let first = self.out.len();
        self.tool_pair(
            tag,
            key,
            started,
            ended,
            "exec_command",
            id,
            Value::Object(input),
            out,
            duration,
        );
        if let Some(source) = str_field(item, "source").filter(|s| is_token(s)) {
            // Both halves of the pair say where the command came from.
            for ev in &mut self.out[first..] {
                tag_provider(ev, "exec_source", &source);
            }
        }
    }

    fn file_change(
        &mut self,
        tag: &str,
        key: &str,
        id: Option<String>,
        started: Option<i64>,
        ended: Option<i64>,
        item: &Value,
    ) {
        let mut changes = Map::new();
        let (mut added, mut removed) = (0u64, 0u64);
        if let Some(map) = item.get("changes").and_then(Value::as_object) {
            for (path, change) in map {
                let kind = change
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("update");
                let mut entry = Map::new();
                entry.insert("type".into(), Value::String(kind.to_string()));
                if let Some(content) = change.get("content").and_then(Value::as_str) {
                    added += crate::common::line_count(content);
                    entry.insert("content".into(), Value::String(content.to_string()));
                }
                if let Some(diff) = change.get("unified_diff").and_then(Value::as_str) {
                    let (a, r) = unified_diff_delta(diff);
                    added += a;
                    removed += r;
                    entry.insert("unified_diff".into(), Value::String(diff.to_string()));
                }
                if let Some(to) = change.get("move_path").and_then(Value::as_str) {
                    entry.insert("move_path".into(), Value::String(to.to_string()));
                }
                changes.insert(path.clone(), Value::Object(entry));
            }
        }
        let paths: Vec<String> = changes.keys().cloned().collect();
        let status = item.get("status").and_then(Value::as_str).unwrap_or("");
        let ending = match status {
            "completed" => Ending::Success,
            "declined" => Ending::Denied,
            "failed" => Ending::Failure,
            _ => Ending::Unknown,
        };
        let text = ["stderr", "stdout"]
            .iter()
            .filter(|_| ending != Ending::Success)
            .find_map(|k| {
                item.get(*k)
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
            })
            .unwrap_or_default()
            .to_string();
        let out = OutputFacts {
            text,
            images: 0,
            ending,
            exit_code: None,
            wall_ms: None,
        };
        let input = serde_json::json!({ "changes": Value::Object(changes) });
        let mut facts = InputFacts::of("apply_patch", &input);
        facts.paths = paths;
        facts.delta = Some((added, removed));
        let (bounded, truncated) = bound_input(&input, 8 * 1024);
        self.stats.tool_calls += 1;
        let name = "apply_patch";
        let call_id = id.clone();
        let started_facts = facts.clone();
        self.emit_event(tag, EventKind::ToolCallStarted, key, started, move |n| {
            n.set_tool(name, call_id.as_deref());
            if let Some(t) = n.event.tool.as_mut() {
                t.category = codex_category(name);
            }
            n.set_tool_input(&bounded);
            if truncated {
                n.provider_attr("input_truncated", true);
            }
            started_facts.apply(n);
        });
        self.tool_finished(tag, key, ended, name, id, &facts, out, None);
    }

    fn mcp_item(
        &mut self,
        tag: &str,
        key: &str,
        id: Option<String>,
        started: Option<i64>,
        ended: Option<i64>,
        item: &Value,
    ) {
        let server = str_field(item, "server").unwrap_or_else(|| "unknown".into());
        let tool = str_field(item, "tool").unwrap_or_else(|| "unknown".into());
        let name = format!("mcp__{}__{}", tag_token(&server), tag_token(&tool));
        let input = item
            .get("arguments")
            .cloned()
            .filter(Value::is_object)
            .unwrap_or_else(|| Value::Object(Map::new()));
        let status = item.get("status").and_then(Value::as_str).unwrap_or("");
        let result = item.get("result").filter(|r| r.is_object());
        let is_error = result
            .and_then(|r| r.get("isError"))
            .and_then(Value::as_bool)
            == Some(true);
        let error = item.get("error").filter(|e| !e.is_null());
        let ending = if status == "failed" || is_error || error.is_some() {
            Ending::Failure
        } else if status == "completed" || result.is_some() {
            Ending::Success
        } else {
            Ending::Unknown
        };
        let (text, images) = match (result, error) {
            (Some(r), _) => mcp_result_text(r),
            (None, Some(e)) => (
                e.get("message")
                    .and_then(Value::as_str)
                    .or_else(|| e.as_str())
                    .unwrap_or_default()
                    .to_string(),
                0,
            ),
            _ => (String::new(), 0),
        };
        let out = OutputFacts {
            text,
            images,
            ending,
            exit_code: None,
            wall_ms: None,
        };
        let duration = duration_ms(item.get("duration"));
        self.tool_pair(tag, key, started, ended, &name, id, input, out, duration);
    }

    fn mcp_tool_call_end(&mut self, pos: &str, p: &Value) {
        let inv = p.get("invocation").filter(|i| i.is_object());
        let server = inv
            .and_then(|i| str_field(i, "server"))
            .unwrap_or_else(|| "unknown".into());
        let tool = inv
            .and_then(|i| str_field(i, "tool"))
            .unwrap_or_else(|| "unknown".into());
        let name = format!("mcp__{}__{}", tag_token(&server), tag_token(&tool));
        let input = inv
            .and_then(|i| i.get("arguments"))
            .cloned()
            .filter(Value::is_object)
            .unwrap_or_else(|| Value::Object(Map::new()));
        let call_id = str_field(p, "call_id");
        let key = call_key(call_id.as_deref(), pos);
        let result = p.get("result");
        let (ending, text, images) = match result {
            Some(r) if r.get("Ok").is_some() => {
                let ok = r.get("Ok").filter(|o| o.is_object());
                let is_error =
                    ok.and_then(|o| o.get("isError")).and_then(Value::as_bool) == Some(true);
                let (text, images) = ok.map(mcp_result_text).unwrap_or_default();
                (
                    if is_error {
                        Ending::Failure
                    } else {
                        Ending::Success
                    },
                    text,
                    images,
                )
            }
            Some(r) if r.get("Err").is_some() => (
                Ending::Failure,
                r.get("Err")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                0,
            ),
            _ => (Ending::Unknown, String::new(), 0),
        };
        let out = OutputFacts {
            text,
            images,
            ending,
            exit_code: None,
            wall_ms: None,
        };
        let duration = duration_ms(p.get("duration"));
        self.tool_pair(
            "event_msg:mcp_tool_call_end",
            &key,
            None,
            None,
            &name,
            call_id,
            input,
            out,
            duration,
        );
    }

    fn dynamic_tool(
        &mut self,
        tag: &str,
        key: &str,
        id: Option<String>,
        started: Option<i64>,
        ended: Option<i64>,
        item: &Value,
    ) {
        let tool = str_field(item, "tool").unwrap_or_else(|| "unknown".into());
        let name = match str_field(item, "namespace") {
            Some(ns) => format!("{}__{}", tag_token(&ns), tag_token(&tool)),
            None => tag_token(&tool),
        };
        let input = item
            .get("arguments")
            .cloned()
            .filter(Value::is_object)
            .unwrap_or_else(|| Value::Object(Map::new()));
        let success = item.get("success").and_then(Value::as_bool);
        let status = item.get("status").and_then(Value::as_str).unwrap_or("");
        let ending = match (success, status) {
            (Some(false), _) | (_, "failed") => Ending::Failure,
            (Some(true), _) | (_, "completed") => Ending::Success,
            _ => Ending::Unknown,
        };
        let text = item
            .get("content_items")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|i| i.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        let out = OutputFacts {
            text,
            images: 0,
            ending,
            exit_code: None,
            wall_ms: None,
        };
        let duration = duration_ms(item.get("duration"));
        self.tool_pair(tag, key, started, ended, &name, id, input, out, duration);
    }

    fn extension(
        &mut self,
        pos: &str,
        tag: &str,
        id: Option<String>,
        started: Option<i64>,
        ended: Option<i64>,
        item: &Value,
    ) {
        match item.get("kind").and_then(Value::as_str).unwrap_or("") {
            "web.search" => {
                let query = str_field(item, "query")
                    .or_else(|| item.get("action").and_then(|a| str_field(a, "query")));
                self.web_search(pos, id, query);
            }
            "image_gen.generation" => {
                // `generating` is progress; the completed item is the fact.
                if item.get("status").and_then(Value::as_str) == Some("completed") {
                    self.image_generation(pos, id, item, ended.or(started));
                } else {
                    self.stats.skipped_entries += 1;
                }
            }
            // Mirrors the `sleep` function call.
            "clock.sleep" => self.stats.skipped_entries += 1,
            _ => self.unknown(tag, pos),
        }
    }

    fn web_search(&mut self, pos: &str, id: Option<String>, query: Option<String>) {
        let key = match &id {
            Some(i) => format!("media:{i}"),
            None => pos.to_string(),
        };
        if !self.remember_media(&key) {
            self.stats.skipped_entries += 1;
            return;
        }
        let input = serde_json::json!({ "query": query.unwrap_or_default() });
        self.tool_pair(
            "web_search",
            &key,
            None,
            None,
            "web_search",
            id,
            input,
            ok_output(),
            None,
        );
    }

    fn image_generation(&mut self, pos: &str, id: Option<String>, p: &Value, ts: Option<i64>) {
        let key = match &id {
            Some(i) => format!("media:{i}"),
            None => pos.to_string(),
        };
        if !self.remember_media(&key) {
            self.stats.skipped_entries += 1;
            return;
        }
        let prompt = str_field(p, "revised_prompt")
            .or_else(|| str_field(p, "revisedPrompt"))
            .unwrap_or_default();
        let saved = str_field(p, "saved_path").or_else(|| str_field(p, "savedPath"));
        let failed = p.get("failure").is_some_and(|f| !f.is_null())
            || matches!(
                p.get("status").and_then(Value::as_str),
                Some("failed" | "error")
            );
        let mut input = Map::new();
        input.insert("prompt".into(), Value::String(prompt));
        if let Some(path) = saved {
            input.insert("path".into(), Value::String(path));
        }
        let out = OutputFacts {
            text: String::new(),
            images: 1,
            ending: if failed {
                Ending::Failure
            } else {
                Ending::Success
            },
            exit_code: None,
            wall_ms: None,
        };
        self.tool_pair(
            "image_generation",
            &key,
            ts,
            ts,
            "image_generation",
            id,
            Value::Object(input),
            out,
            None,
        );
    }

    /// True the first time a media id is seen (bounded memory).
    fn remember_media(&mut self, key: &str) -> bool {
        if self.seen_media.len() >= MAX_SEEN_MEDIA {
            self.seen_media.clear();
        }
        self.seen_media.insert(key.to_string())
    }

    // --- compaction ---------------------------------------------------------

    fn compacted(&mut self, pos: &str, p: &Value) {
        self.stats.compactions += 1;
        let summary = p
            .get("message")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|s| bound_text(s, MAX_TEXT).0);
        let window = p.get("window_number").and_then(Value::as_u64);
        let replaced = p
            .get("replacement_history")
            .and_then(Value::as_array)
            .map(|a| a.len() as u64);
        self.emit_event(
            "compacted",
            EventKind::CompactionFinished,
            pos,
            None,
            move |n| {
                n.attr("trigger", "transcript_compacted");
                if let Some(w) = window {
                    n.provider_attr("window_number", w);
                }
                if let Some(r) = replaced {
                    n.provider_attr("replacement_items", r);
                }
                if let Some(s) = summary {
                    n.set_extra("summary", s);
                }
            },
        );
    }

    // --- lines too large to parse -------------------------------------------

    fn oversized(&mut self, line_no: usize, hint: &Hint<'_>, info: &LineInfo) {
        self.stats.oversized_lines += 1;
        self.stats.entries += 1;
        let pos = match hint.ordinal {
            Some(o) => format!("o{o}"),
            None => format!("l{line_no}"),
        };
        self.last_pos.clone_from(&pos);
        self.warn(
            line_no,
            &format!(
                "line of {} bytes is over the {} byte limit; kept only what its first bytes say ({})",
                info.total_len,
                self.opts.max_line_bytes,
                match (hint.top, hint.sub) {
                    (Some(t), Some(s)) => format!("{t}/{s}"),
                    (Some(t), None) => t.to_string(),
                    _ => "type unknown".to_string(),
                }
            ),
        );
        self.ensure_started(line_no);
        match (hint.top, hint.sub) {
            (Some("compacted"), _) => {
                self.stats.compactions += 1;
                self.emit_event(
                    "compacted",
                    EventKind::CompactionFinished,
                    &pos,
                    None,
                    |n| {
                        n.attr("trigger", "transcript_compacted");
                        n.provider_attr("oversized", true);
                    },
                );
            }
            (Some("response_item"), Some("function_call_output" | "custom_tool_call_output")) => {
                let call_id = hint.call_id.map(str::to_string);
                let sub = hint.sub.unwrap_or("function_call_output").to_string();
                let (name, facts, started_ms) =
                    match call_id.as_ref().and_then(|c| self.tools.remove(c)) {
                        Some(p) => (p.name, p.facts, p.started_ms),
                        None => ("unknown".to_string(), InputFacts::default(), None),
                    };
                let key = call_key(call_id.as_deref(), &pos);
                let duration = self.elapsed_since(started_ms);
                let out = OutputFacts {
                    text: String::new(),
                    images: 0,
                    ending: Ending::Unknown,
                    exit_code: None,
                    wall_ms: None,
                };
                self.tool_finished(
                    &format!("response_item:{sub}"),
                    &key,
                    None,
                    &name,
                    call_id,
                    &facts,
                    out,
                    duration,
                );
                if let Some(ev) = self.out.last_mut() {
                    ev.attrs
                        .insert("tool_output_truncated".into(), Value::Bool(true));
                }
            }
            _ => {}
        }
    }

    // --- finish -------------------------------------------------------------

    fn finish(&mut self) {
        if !self.opts.emit_session_end || !self.started {
            return;
        }
        let key = format!("end:{}", self.last_pos);
        let usage = self.total_usage;
        if self.is_subagent() {
            self.emit_event(
                "session_meta:subagent_end",
                EventKind::SubagentStopped,
                &key,
                None,
                |_| {},
            );
        } else {
            self.emit_event(
                "session_end",
                EventKind::SessionEnded,
                &key,
                None,
                move |n| {
                    n.attr("source", "transcript");
                    if let Some(u) = usage {
                        n.provider_attr("session_total_tokens", u.total);
                        n.provider_attr("session_input_tokens", u.input);
                        n.provider_attr("session_output_tokens", u.output);
                    }
                },
            );
        }
    }
}

impl Encoding {
    fn tag(self) -> &'static str {
        match self {
            Encoding::EventMsg => "event_msg",
            Encoding::Item => "event_msg:item_completed",
            Encoding::ResponseItem => "response_item",
        }
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// The class Codex names for a failed turn: `error.codex_error_info`, a
/// token (`usage_limit_exceeded`) or an object keyed by one.
fn error_class_of(error: &Value) -> Option<String> {
    match error.get("codex_error_info")? {
        Value::String(s) if is_token(s) => Some(to_snake(s)),
        Value::Object(o) => o.keys().next().filter(|k| is_token(k)).map(|k| to_snake(k)),
        _ => None,
    }
}

fn session_attrs(n: &mut Normaliser<'_>, meta: &RolloutMeta) {
    if let Some(o) = &meta.originator {
        n.attr("entrypoint", o.as_str());
    }
    if let Some(s) = &meta.source {
        n.provider_attr("cli_source", s.as_str());
    }
    if let Some(a) = &meta.agent_type {
        n.provider_attr("agent_role", a.as_str());
    }
}

fn parse_value(line: &[u8]) -> Option<Value> {
    serde_json::from_slice(line).ok().or_else(|| {
        // Invalid UTF-8 inside a string: keep the line, replace the bytes.
        let text = String::from_utf8_lossy(line);
        serde_json::from_str(&text).ok()
    })
}

/// Top-level `type`s a pre-envelope rollout wrote bare.
fn is_response_item_type(t: &str) -> bool {
    matches!(
        t,
        "message"
            | "function_call"
            | "function_call_output"
            | "custom_tool_call"
            | "custom_tool_call_output"
            | "local_shell_call"
            | "local_shell_call_output"
            | "reasoning"
    )
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn token_field(v: &Value, key: &str) -> Option<String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| is_token(s))
        .map(to_snake)
}

fn count_list(v: Option<&Value>) -> usize {
    v.and_then(Value::as_array).map_or(0, Vec::len)
}

/// A provider type name made safe to put in an event name: ASCII
/// alphanumerics, `_`, `.` and `-` only, bounded.
fn tag_token(s: &str) -> String {
    s.chars()
        .take(48)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn call_key(call_id: Option<&str>, pos: &str) -> String {
    match call_id {
        Some(id) => tool_call_key(id),
        None => pos.to_string(),
    }
}

fn tag_provider(ev: &mut Event, key: &str, value: &str) {
    let entry = ev
        .attrs
        .entry("provider".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(map) = entry.as_object_mut() {
        map.insert(key.to_string(), Value::String(value.to_string()));
    }
}

/// Text of a message `content` list: the `text` of every part, joined.
fn content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// `{secs, nanos}` as milliseconds.
fn duration_ms(v: Option<&Value>) -> Option<u64> {
    let d = v?;
    let secs = d.get("secs").and_then(Value::as_u64)?;
    let nanos = d.get("nanos").and_then(Value::as_u64).unwrap_or(0);
    Some(secs.saturating_mul(1000) + nanos / 1_000_000)
}

/// The shell script of a command value: a string, or an argv array whose
/// last element is the script of a `-c`/`-lc` shell wrapper (otherwise the
/// words joined).
pub fn script_of(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Array(parts) => {
            let words: Vec<&str> = parts.iter().filter_map(Value::as_str).collect();
            let wrapper = words.len() >= 3
                && words[words.len() - 2].starts_with('-')
                && words[words.len() - 2].ends_with('c');
            if wrapper {
                return words.last().map(|s| (*s).to_string());
            }
            (!words.is_empty()).then(|| words.join(" "))
        }
        _ => None,
    }
}

/// Lines added and removed by an `apply_patch` envelope: hunk lines
/// starting with `+` / `-` (file headers start with `***`).
pub fn patch_line_delta(patch: &str) -> (u64, u64) {
    let (mut added, mut removed) = (0, 0);
    for line in patch.lines() {
        if line.starts_with("***") {
            continue;
        }
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    (added, removed)
}

/// Lines added and removed by a unified diff body.
pub fn unified_diff_delta(diff: &str) -> (u64, u64) {
    let (mut added, mut removed) = (0, 0);
    for line in diff.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    (added, removed)
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

/// A copy of `v` whose strings are cut to `max` bytes.
fn bound_input(v: &Value, max: usize) -> (Value, bool) {
    fn walk(v: &Value, max: usize, cut: &mut bool) -> Value {
        match v {
            Value::String(s) if s.len() > max => {
                *cut = true;
                Value::String(bound_text(s, max).0)
            }
            Value::Array(a) => Value::Array(a.iter().map(|x| walk(x, max, cut)).collect()),
            Value::Object(o) => Value::Object(
                o.iter()
                    .map(|(k, x)| (k.clone(), walk(x, max, cut)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    let mut cut = false;
    let out = walk(v, max, &mut cut);
    (out, cut)
}

fn ok_output() -> OutputFacts {
    OutputFacts {
        text: String::new(),
        images: 0,
        ending: Ending::Success,
        exit_code: None,
        wall_ms: None,
    }
}

/// Text and image count of an MCP result (`content` parts).
fn mcp_result_text(result: &Value) -> (String, usize) {
    let Some(parts) = result.get("content").and_then(Value::as_array) else {
        return (String::new(), 0);
    };
    let mut images = 0;
    let mut texts = Vec::new();
    for p in parts {
        match p.get("type").and_then(Value::as_str) {
            Some("image") => images += 1,
            _ => {
                if let Some(t) = p.get("text").and_then(Value::as_str) {
                    texts.push(t);
                }
            }
        }
    }
    (texts.join("\n"), images)
}

/// Read what a tool output says: its text, images, and how the call went.
fn interpret_output(name: &str, output: &Value) -> OutputFacts {
    let (mut text, images) = match output {
        Value::String(s) => (s.clone(), 0),
        Value::Array(parts) => {
            let mut images = 0;
            let mut texts = Vec::new();
            for p in parts {
                match p.get("type").and_then(Value::as_str) {
                    Some("input_image" | "output_image" | "image") => images += 1,
                    _ => {
                        if let Some(t) = p.get("text").and_then(Value::as_str) {
                            texts.push(t);
                        }
                    }
                }
            }
            (texts.join("\n"), images)
        }
        Value::Null => (String::new(), 0),
        other => (other.to_string(), 0),
    };
    let mut exit_code = None;
    let mut wall_ms = None;
    // `apply_patch` answers with `{"output": "...", "metadata": {"exit_code": n}}`.
    if (name == "apply_patch" || text.starts_with("{\"output\""))
        && text.len() < 1 << 20
        && text.starts_with('{')
        && let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&text)
        && let Some(inner) = map.get("output").and_then(Value::as_str)
    {
        exit_code = map
            .get("metadata")
            .and_then(|m| m.get("exit_code"))
            .and_then(Value::as_i64)
            .and_then(|c| i32::try_from(c).ok());
        wall_ms = map
            .get("metadata")
            .and_then(|m| m.get("duration_seconds"))
            .and_then(Value::as_f64)
            .filter(|s| s.is_finite() && *s >= 0.0)
            .map(|s| (s * 1000.0) as u64);
        text = inner.to_string();
    }
    let mut ending = Ending::Success;
    let mut running = false;
    for (i, raw) in text.lines().take(8).enumerate() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("Process exited with code ") {
            exit_code = rest.trim().parse().ok().or(exit_code);
        } else if let Some(rest) = line.strip_prefix("Exit code: ") {
            exit_code = rest.trim().parse().ok().or(exit_code);
        } else if let Some(rest) = line
            .strip_prefix("Wall time: ")
            .or_else(|| line.strip_prefix("Wall time "))
        {
            let secs = rest.trim_end_matches("seconds").trim();
            if let Ok(s) = secs.parse::<f64>()
                && s.is_finite()
                && s >= 0.0
            {
                wall_ms = Some((s * 1000.0) as u64);
            }
        } else if line.starts_with("Process running with session ID")
            || line.starts_with("Script running with cell ID")
        {
            running = true;
        } else if i == 0 {
            let lower = line.to_ascii_lowercase();
            if line == "Script failed" {
                ending = Ending::Failure;
            } else if lower.starts_with("aborted by user") {
                ending = Ending::Cancelled;
            } else if lower.starts_with("failed to ")
                || lower.starts_with("error:")
                || lower.starts_with("write_stdin failed")
            {
                ending = Ending::Failure;
            }
        }
    }
    if ending == Ending::Success {
        if let Some(code) = exit_code
            && code != 0
        {
            ending = Ending::Failure;
        } else if running && exit_code.is_none() {
            ending = Ending::Running;
        }
    }
    OutputFacts {
        text,
        images,
        ending,
        exit_code,
        wall_ms,
    }
}

/// Category of a Codex tool name (Codex's own vocabulary on top of the
/// shared classifier).
fn codex_category(name: &str) -> ToolCategory {
    match name {
        "exec_command" | "write_stdin" | "shell" | "shell_command" | "local_shell"
        | "container.exec" | "unified_exec" => ToolCategory::Shell,
        "view_image" => ToolCategory::FileRead,
        "apply_patch" => ToolCategory::FileEdit,
        "web_search" | "web.search" => ToolCategory::Web,
        "spawn_agent" | "send_message" | "wait_agent" | "followup_task" | "interrupt_agent"
        | "list_agents" | "close_agent" | "resume_agent" => ToolCategory::Subagent,
        "update_plan" => ToolCategory::Plan,
        other => classify_tool(other),
    }
}

#[cfg(test)]
mod unit {
    use super::*;
    use serde_json::json;

    #[test]
    fn hints_read_the_envelope_prefix() {
        let line = br#"{"timestamp":"2026-09-28T07:46:11.789Z","ordinal":12,"type":"response_item","payload":{"type":"custom_tool_call_output","call_id":"call_x1","output":[{"type":"input_text","text":"Script completed"#;
        let h = Hint::of(line);
        assert_eq!(h.ts, Some("2026-09-28T07:46:11.789Z"));
        assert_eq!(h.ordinal, Some(12));
        assert_eq!(h.top, Some("response_item"));
        assert_eq!(h.sub, Some("custom_tool_call_output"));
        assert_eq!(h.call_id, Some("call_x1"));
        assert!(!h.is_bookkeeping());

        let old = br#"{"timestamp":"2026-06-26T13:25:18.622Z","type":"session_meta","payload":{"id":"a"}}"#;
        let h = Hint::of(old);
        assert_eq!(
            (h.ordinal, h.top, h.sub),
            (None, Some("session_meta"), None)
        );

        let book = br#"{"timestamp":"2026-06-26T13:25:18.622Z","type":"response_item","payload":{"type":"reasoning","summary":[]}}"#;
        assert!(Hint::of(book).is_bookkeeping());
        assert!(Hint::of(b"garbage").top.is_none());
        assert!(Hint::of(br#"{"timestamp":"2026"#).top.is_none());
    }

    #[test]
    fn scripts_come_out_of_shell_wrappers() {
        assert_eq!(
            script_of(&json!(["/bin/zsh", "-lc", "cargo test -p x"])).as_deref(),
            Some("cargo test -p x")
        );
        assert_eq!(
            script_of(&json!(["git", "status"])).as_deref(),
            Some("git status")
        );
        assert_eq!(script_of(&json!("ls -la")).as_deref(), Some("ls -la"));
        assert_eq!(script_of(&json!([])), None);
        assert_eq!(script_of(&json!(7)), None);
    }

    #[test]
    fn patch_and_diff_deltas() {
        let patch = "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-old\n+new1\n+new2\n context\n*** Add File: b.rs\n+x\n*** End Patch";
        assert_eq!(patch_line_delta(patch), (3, 1));
        let diff = "--- a/f\n+++ b/f\n@@ -1,2 +1,2 @@\n-a\n+b\n c\n";
        assert_eq!(unified_diff_delta(diff), (1, 1));
    }

    #[test]
    fn outputs_are_interpreted() {
        let ok = interpret_output(
            "exec_command",
            &json!(
                "Chunk ID: ab12\nWall time: 0.25 seconds\nProcess exited with code 0\nOriginal token count: 3\nOutput:\nhi"
            ),
        );
        assert_eq!(
            (ok.ending, ok.exit_code, ok.wall_ms),
            (Ending::Success, Some(0), Some(250))
        );
        let bad = interpret_output(
            "exec_command",
            &json!(
                "Chunk ID: ab12\nWall time: 1 seconds\nProcess exited with code 2\nOutput:\nboom"
            ),
        );
        assert_eq!((bad.ending, bad.exit_code), (Ending::Failure, Some(2)));
        let running = interpret_output(
            "exec_command",
            &json!(
                "Chunk ID: ab12\nWall time: 1 seconds\nProcess running with session ID 7\nOutput:\n"
            ),
        );
        assert_eq!(running.ending, Ending::Running);
        let script = interpret_output(
            "exec",
            &json!([
                {"type": "input_text", "text": "Script failed"},
                {"type": "input_text", "text": "Wall time 1.2 seconds"},
                {"type": "input_image", "image_url": "data:"}
            ]),
        );
        assert_eq!((script.ending, script.images), (Ending::Failure, 1));
        let aborted = interpret_output("exec", &json!("aborted by user after 3s"));
        assert_eq!(aborted.ending, Ending::Cancelled);
        let patch = interpret_output(
            "apply_patch",
            &json!(
                r#"{"output":"Success. Updated the following files:\nM a.rs","metadata":{"exit_code":0,"duration_seconds":0.5}}"#
            ),
        );
        assert_eq!(
            (patch.ending, patch.exit_code, patch.wall_ms),
            (Ending::Success, Some(0), Some(500))
        );
        assert!(patch.text.starts_with("Success."));
        let failed_patch = interpret_output(
            "apply_patch",
            &json!("Exit code: 1\nWall time: 0.1 seconds\nOutput:\nnope"),
        );
        assert_eq!(failed_patch.ending, Ending::Failure);
        assert_eq!(interpret_output("x", &Value::Null).ending, Ending::Success);
    }

    #[test]
    fn meta_resolves_the_session_and_subagents() {
        let top = json!({
            "id": "t-1", "session_id": "t-1", "cwd": "/home/dev/p", "cli_version": "0.154.0",
            "source": "cli", "originator": "codex-tui",
            "git": {"branch": "main", "repository_url": "git@github.com:example/project.git", "commit_hash": "abc"}
        });
        let m = RolloutMeta::from_payload(&top);
        assert_eq!(m.session_id.as_deref(), Some("t-1"));
        assert!(!m.is_subagent());
        assert_eq!(m.git_branch.as_deref(), Some("main"));
        assert_eq!(m.originator.as_deref(), Some("codex_tui"));
        let sub = json!({
            "id": "t-2", "session_id": "t-1", "parent_thread_id": "t-1",
            "source": {"subagent": {"other": "guardian"}}, "subagent_history_start_ordinal": 14
        });
        let m = RolloutMeta::from_payload(&sub);
        assert_eq!(m.session_id.as_deref(), Some("t-1"));
        assert_eq!(m.thread_id.as_deref(), Some("t-2"));
        assert!(m.is_subagent());
        assert_eq!(m.agent_type.as_deref(), Some("guardian"));
        assert_eq!(m.history_start_ordinal, Some(14));
        let spawn = json!({
            "id": "t-3", "parent_thread_id": "t-1",
            "source": {"subagent": {"thread_spawn": {"agent_role": "explorer", "agent_nickname": "Zeno"}}}
        });
        assert_eq!(
            RolloutMeta::from_payload(&spawn).agent_type.as_deref(),
            Some("explorer")
        );
    }

    #[test]
    fn id_derivation_is_one_pure_function() {
        let a = rollout_event_id("s", "call:c1", EventKind::ToolCallStarted);
        assert_eq!(
            a,
            rollout_event_id("s", "call:c1", EventKind::ToolCallStarted)
        );
        assert_ne!(
            a,
            rollout_event_id("s", "call:c1", EventKind::ToolCallFinished)
        );
        assert_ne!(
            a,
            rollout_event_id("s", "call:c2", EventKind::ToolCallStarted)
        );
        assert_ne!(
            a,
            rollout_event_id("t", "call:c1", EventKind::ToolCallStarted)
        );
    }

    #[test]
    fn bounding_respects_char_boundaries() {
        let (s, cut) = bound_text("한글한글", 4);
        assert!(cut);
        assert_eq!(s, "한");
        let (v, cut) = bound_input(&json!({"a": "x".repeat(10), "b": [1, "y".repeat(10)]}), 4);
        assert!(cut);
        assert_eq!(v, json!({"a": "xxxx", "b": [1, "yyyy"]}));
    }

    #[test]
    fn the_line_reader_caps_memory_and_reports_partial_tails() {
        let data = format!("short\n{}\nlast", "x".repeat(10_000));
        let mut r = LineReader::new(std::io::Cursor::new(data.into_bytes()), 5000);
        let a = r.next_line().unwrap().unwrap();
        assert_eq!(
            (r.bytes(), a.oversized, a.terminated),
            (&b"short"[..], false, true)
        );
        let b = r.next_line().unwrap().unwrap();
        assert!(b.oversized && b.terminated);
        assert_eq!(b.total_len, 10_000);
        assert!(r.bytes().len() <= HINT_PREFIX_BYTES);
        let c = r.next_line().unwrap().unwrap();
        assert_eq!((r.bytes(), c.terminated), (&b"last"[..], false));
        assert!(r.next_line().unwrap().is_none());
    }
}
