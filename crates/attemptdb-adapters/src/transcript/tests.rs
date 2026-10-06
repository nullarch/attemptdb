//! Fixture-driven contract tests for the Claude Code transcript parser.
//!
//! Fixtures live in `fixtures/transcripts/claude_code/<name>.jsonl`; each has
//! a golden envelope list next to it (`<name>.golden.json`) generated when
//! missing and regenerated with `UPDATE_GOLDEN=1`. Every content-bearing
//! string in the fixtures carries a `CANARY_` marker so leaks into metadata
//! are caught by a plain substring search.

use super::claude_code::{TranscriptImport, TranscriptOptions, parse_claude_transcript};
use crate::CaptureContext;
use crate::common::{ALLOWED_ATTR_KEYS, derive_event_id, tool_call_key};
use attemptdb_core::event::Provider;
use attemptdb_core::{
    AgentId, CaptureMode, DeviceId, Event, EventId, EventKind, OutcomeStatus, ProjectRef,
    SessionId, Timestamp, ToolCategory,
};
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

const PROJECT_ROOT: &str = "/home/dev/example/project";
const PROJECT_REMOTE: &str = "git@github.com:example/project.git";
const CAPTURED_AT: Timestamp = Timestamp::from_micros(1_787_904_000_000_000); // 2026-08-28T08:00:00Z
const CANARY: &str = "CANARY_";

const FIXTURES: &[&str] = &[
    "basic_turn",
    "subagent_sidechain",
    "compaction_summary",
    "interrupted_turn",
    "mixed_edge_cases",
    "queued_and_usage",
];

use EventKind::*;

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/transcripts/claude_code")
}

fn device_id() -> DeviceId {
    DeviceId::derive(&["transcript-tests"])
}

fn context(mode: CaptureMode) -> CaptureContext {
    let device_id = device_id();
    CaptureContext {
        device_id,
        capture_mode: mode,
        project: ProjectRef::derive(PROJECT_ROOT, Some(PROJECT_REMOTE), &device_id),
        captured_at: CAPTURED_AT,
        provider_version: None,
        hook_version: Some("must-not-appear".into()),
    }
}

fn options_for(name: &str, include_content: bool) -> TranscriptOptions {
    let mut opts = TranscriptOptions {
        include_content,
        ..TranscriptOptions::default()
    };
    opts.session_id_hint = Some(format!("{name}-stem"));
    if name == "subagent_sidechain" {
        opts.agent_type_hint = Some("Explore".into());
        opts.parent_tool_use_id = Some("toolu_0003".into());
    }
    opts
}

fn lines(name: &str) -> Vec<String> {
    let path = fixtures_dir().join(format!("{name}.jsonl"));
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
        .lines()
        .map(str::to_string)
        .collect()
}

fn parse(name: &str, mode: CaptureMode, include_content: bool) -> TranscriptImport {
    parse_claude_transcript(
        lines(name).into_iter(),
        &context(mode),
        &options_for(name, include_content),
    )
}

fn parse_lines(name: &str, lines: Vec<String>) -> TranscriptImport {
    parse_claude_transcript(
        lines.into_iter(),
        &context(CaptureMode::LocalSemantic),
        &options_for(name, true),
    )
}

fn kinds(import: &TranscriptImport) -> Vec<EventKind> {
    import.events.iter().map(|e| e.kind).collect()
}

fn attr<'a>(ev: &'a Event, key: &str) -> &'a Value {
    ev.attrs.get(key).unwrap_or(&Value::Null)
}

fn provider_attr<'a>(ev: &'a Event, key: &str) -> &'a Value {
    ev.attrs
        .get("provider")
        .and_then(|p| p.get(key))
        .unwrap_or(&Value::Null)
}

fn zeroed_for_golden(mut event: Event) -> Event {
    event.device_id = DeviceId::nil();
    event.captured_at = Timestamp::from_micros(0);
    event
}

// ---------------------------------------------------------------------------
// Provenance shared by every event
// ---------------------------------------------------------------------------

#[test]
fn every_event_is_marked_reconstructed() {
    for name in FIXTURES {
        let import = parse(name, CaptureMode::LocalSemantic, true);
        assert!(!import.events.is_empty(), "{name}: no events");
        let session = import
            .provider_session_id
            .clone()
            .expect("session id found");
        let expected_session = SessionId::derive(&[Provider::ClaudeCode.as_str(), &session]);
        for ev in &import.events {
            let label = format!("{name}/{}", ev.provider_event_name);
            assert_eq!(ev.provider, Provider::ClaudeCode, "{label}");
            assert_eq!(ev.adapter_version, crate::ADAPTER_VERSION, "{label}");
            assert_eq!(ev.hook_version, None, "{label}: hook_version");
            assert_eq!(ev.raw, None, "{label}: raw");
            assert_eq!(ev.captured_at, CAPTURED_AT, "{label}: captured_at");
            assert_eq!(ev.provider_session_id, session, "{label}: session");
            assert_eq!(ev.session_id, expected_session, "{label}: derived session");
            assert!(
                ev.provider_event_name.starts_with("transcript:"),
                "{label}: name"
            );
            assert_eq!(attr(ev, "reconstructed"), &Value::Bool(true), "{label}");
            assert_eq!(
                attr(ev, "reconstructed_from"),
                "claude_code_transcript",
                "{label}"
            );
            assert_eq!(
                attr(ev, "transcript_present"),
                &Value::Bool(true),
                "{label}"
            );
            assert!(
                attr(ev, "transcript_entry_type").is_string(),
                "{label}: entry type"
            );
            assert!(
                ev.attrs.get("hook_event_name").is_none(),
                "{label}: hook_event_name"
            );
            assert_ne!(
                ev.observed_at,
                Timestamp::from_micros(0),
                "{label}: observed_at"
            );
            for key in ev.attrs.keys() {
                assert!(
                    ALLOWED_ATTR_KEYS.contains(&key.as_str()),
                    "{label}: attr `{key}` not allowlisted"
                );
            }
        }
    }
}

#[test]
fn ids_are_deterministic_and_unique() {
    for name in FIXTURES {
        let a = parse(name, CaptureMode::LocalSemantic, true);
        let b = parse(name, CaptureMode::MetadataOnly, false);
        let ids_a: Vec<EventId> = a.events.iter().map(|e| e.event_id).collect();
        let ids_b: Vec<EventId> = b.events.iter().map(|e| e.event_id).collect();
        assert_eq!(ids_a, ids_b, "{name}: ids depend on the transcript only");
        let unique: HashSet<EventId> = ids_a.iter().copied().collect();
        assert_eq!(unique.len(), ids_a.len(), "{name}: duplicate ids");
        assert!(ids_a.iter().all(|id| !id.is_nil()));
    }
}

// ---------------------------------------------------------------------------
// basic_turn
// ---------------------------------------------------------------------------

#[test]
fn basic_turn_kinds_and_order() {
    let import = parse("basic_turn", CaptureMode::LocalSemantic, true);
    assert_eq!(
        kinds(&import),
        vec![
            SessionStarted,
            PromptSubmitted,
            // The narration before the first tool call.
            AgentMessage,
            ToolCallStarted,
            ToolCallFinished,
            ToolCallStarted,
            ToolCallFailed,
            AgentMessage,
            TurnStopped,
            PromptSubmitted,
            AgentMessage,
            TurnStopped,
        ]
    );
    assert!(import.warnings.is_empty(), "{:?}", import.warnings);
    let s = &import.stats;
    assert_eq!(
        (s.entries, s.prompts, s.tool_calls, s.tool_failures, s.turns),
        (19, 2, 2, 1, 2)
    );
    assert_eq!(
        (s.interim_messages, s.queued_prompts, s.api_errors),
        (1, 0, 0)
    );
    assert_eq!(
        (s.unknown_entries, s.malformed_lines, s.subagent_entries),
        (0, 0, 0)
    );
    assert_eq!(s.skipped_entries, 10);
    assert_eq!(
        import.provider_session_id.as_deref(),
        Some("11111111-1111-4111-8111-111111111111")
    );

    let start = &import.events[0];
    assert_eq!(start.provider_event_name, "transcript:session_start");
    assert_eq!(attr(start, "source"), "transcript");
    assert_eq!(
        start.observed_at,
        Timestamp::parse("2026-08-20T09:00:00.000Z").unwrap()
    );
    assert_eq!(start.provider_version.as_deref(), Some("2.1.190"));
    // `attrs.cwd` is home-elided (RFC 0006 §4.2); `PROJECT_ROOT` is `/home/dev/…`.
    assert_eq!(attr(start, "cwd"), "~/example/project");
    assert_eq!(attr(start, "turn_index_hint"), 0);

    let prompt = &import.events[1];
    assert_eq!(prompt.provider_event_name, "transcript:user");
    assert_eq!(
        prompt.provider_turn_id.as_deref(),
        Some("p0000001-0000-4000-8000-000000000000")
    );
    assert_eq!(attr(prompt, "turn_index_hint"), 1);
    assert_eq!(attr(prompt, "prompt_has_question"), &Value::Bool(true));
    assert_eq!(provider_attr(prompt, "prompt_source"), "typed");
    assert_eq!(provider_attr(prompt, "prompt_kind"), "text");
    assert!(
        prompt
            .content
            .as_ref()
            .unwrap()
            .prompt
            .as_deref()
            .unwrap()
            .starts_with("CANARY_PROMPT_ONE")
    );
    assert_eq!(
        prompt.project.branch.as_deref(),
        Some("main"),
        "gitBranch fills the branch the context lacks"
    );
    assert!(
        import
            .events
            .iter()
            .all(|e| e.provider_version.as_deref() == Some("2.1.190"))
    );
}

#[test]
fn basic_turn_project_branch_comes_from_transcript_when_ctx_lacks_it() {
    let import = parse("basic_turn", CaptureMode::LocalSemantic, true);
    // The test context derives its project without a branch, so the
    // transcript's gitBranch is used.
    assert!(
        import
            .events
            .iter()
            .all(|e| e.project.branch.as_deref() == Some("main") || e.project.branch.is_none())
    );
    assert!(
        import
            .events
            .iter()
            .any(|e| e.project.branch.as_deref() == Some("main"))
    );
    let mut ctx = context(CaptureMode::LocalSemantic);
    ctx.project.branch = Some("from-ctx".into());
    let import = parse_claude_transcript(
        lines("basic_turn").into_iter(),
        &ctx,
        &options_for("basic_turn", true),
    );
    assert!(
        import
            .events
            .iter()
            .all(|e| e.project.branch.as_deref() == Some("from-ctx"))
    );
}

#[test]
fn tool_results_pair_with_calls() {
    let import = parse("basic_turn", CaptureMode::LocalSemantic, true);
    let ev = &import.events;
    let bash_start = &ev[3];
    let bash_end = &ev[4];
    assert_eq!(
        bash_start.provider_event_name,
        "transcript:assistant:tool_use"
    );
    assert_eq!(bash_end.provider_event_name, "transcript:user:tool_result");
    let t = bash_start.tool.as_ref().unwrap();
    assert_eq!(
        (t.name.as_str(), t.category, t.call_id.as_deref()),
        ("Bash", ToolCategory::Shell, Some("toolu_0001"))
    );
    let t = bash_end.tool.as_ref().unwrap();
    assert_eq!(
        (t.name.as_str(), t.category, t.call_id.as_deref()),
        ("Bash", ToolCategory::Shell, Some("toolu_0001"))
    );
    assert_eq!(attr(bash_start, "command_category"), "test");
    assert_eq!(bash_start.agent.model.as_deref(), Some("claude-opus-4-8"));
    assert_eq!(
        bash_start.content.as_ref().unwrap().command.as_deref(),
        Some("cargo test -p example")
    );
    assert_eq!(
        bash_end.outcome.as_ref().map(|o| o.status),
        Some(OutcomeStatus::Success)
    );
    assert!(
        bash_end
            .content
            .as_ref()
            .unwrap()
            .tool_output
            .as_ref()
            .unwrap()
            .as_str()
            .unwrap()
            .starts_with("CANARY_TOOL_OUTPUT")
    );
    assert_eq!(
        bash_end.observed_at,
        Timestamp::parse("2026-08-20T09:00:05.000Z").unwrap()
    );

    let edit_start = &ev[5];
    let edit_end = &ev[6];
    assert_eq!(
        edit_start.tool.as_ref().unwrap().call_id.as_deref(),
        Some("toolu_0002")
    );
    assert_eq!(
        edit_start
            .paths
            .iter()
            .map(|p| p.display())
            .collect::<Vec<_>>(),
        vec!["src/lib.rs"]
    );
    assert_eq!(attr(edit_start, "lines_added"), 1);
    assert_eq!(attr(edit_start, "lines_removed"), 1);
    assert_eq!(edit_end.kind, ToolCallFailed);
    let o = edit_end.outcome.as_ref().unwrap();
    assert_eq!(
        (o.status, o.class.as_deref()),
        (OutcomeStatus::Failure, Some("string_mismatch"))
    );
    assert_eq!(attr(edit_end, "error_class"), "string_mismatch");
    assert_eq!(edit_end.tool.as_ref().unwrap().name, "Edit");
    assert_eq!(
        edit_end
            .paths
            .iter()
            .map(|p| p.display())
            .collect::<Vec<_>>(),
        vec!["src/lib.rs"]
    );
    assert!(
        edit_end
            .content
            .as_ref()
            .unwrap()
            .error
            .as_deref()
            .unwrap()
            .contains("String to replace not found")
    );
    // Same agent on both sides, so projections can pair by call id.
    assert_eq!(edit_start.agent.agent_id, edit_end.agent.agent_id);
}

#[test]
fn turn_end_is_synthesised_before_next_prompt_and_at_eof() {
    let import = parse("basic_turn", CaptureMode::LocalSemantic, true);
    let ev = &import.events;
    let message = &ev[7];
    let stop = &ev[8];
    assert_eq!(message.provider_event_name, "transcript:assistant:text");
    assert_eq!(stop.provider_event_name, "transcript:turn_end");
    assert_eq!(message.observed_at, stop.observed_at);
    assert_eq!(
        message.observed_at,
        Timestamp::parse("2026-08-20T09:00:09.000Z").unwrap()
    );
    assert_eq!(
        stop.duration_ms,
        Some(9000),
        "durationMs of the turn_duration entry"
    );
    assert_eq!(provider_attr(stop, "stop_reason"), "end_turn");
    let text = message.content.as_ref().unwrap().message.clone().unwrap();
    assert_eq!(
        provider_attr(message, "message_chars"),
        text.chars().count() as u64
    );
    assert_eq!(provider_attr(message, "output_tokens"), 42);
    assert_eq!(message.agent.model.as_deref(), Some("claude-opus-4-8"));
    assert!(
        message
            .content
            .as_ref()
            .unwrap()
            .message
            .as_deref()
            .unwrap()
            .starts_with("CANARY_FINAL_MESSAGE")
    );
    assert!(
        message.attrs.get("message_chars").is_none(),
        "message_chars lives under attrs.provider"
    );
    // Narration followed by a tool call is kept, marked interim: it is the
    // agent's own words and no hook carries it. The final message is not.
    let interim = ev
        .iter()
        .filter(|e| provider_attr(e, "interim") == &Value::Bool(true))
        .collect::<Vec<_>>();
    assert_eq!(interim.len(), 1);
    assert_eq!(interim[0].kind, AgentMessage);
    assert!(
        interim[0]
            .content
            .as_ref()
            .unwrap()
            .message
            .as_deref()
            .unwrap()
            .starts_with("CANARY_INTERIM_TEXT")
    );
    assert_eq!(provider_attr(interim[0], "stop_reason"), "tool_use");
    assert_eq!(provider_attr(message, "interim"), &Value::Null);

    let last_stop = ev.last().unwrap();
    assert_eq!(last_stop.kind, TurnStopped);
    assert_eq!(
        last_stop.observed_at,
        Timestamp::parse("2026-08-20T09:01:03.000Z").unwrap()
    );
    assert_eq!(last_stop.duration_ms, None);
    assert_eq!(attr(last_stop, "turn_index_hint"), 2);
    assert_eq!(
        last_stop.provider_turn_id.as_deref(),
        Some("p0000002-0000-4000-8000-000000000000")
    );
}

#[test]
fn growing_transcript_only_adds_events() {
    let all = lines("basic_turn");
    // Cut right after the `turn_duration` entry: turn one is complete.
    let prefix: Vec<String> = all[..16].to_vec();
    let before = parse_lines("basic_turn", prefix);
    let after = parse_lines("basic_turn", all);
    assert_eq!(before.events.len(), 9);
    assert_eq!(after.events.len(), 12);
    let after_ids: HashSet<EventId> = after.events.iter().map(|e| e.event_id).collect();
    for ev in &before.events {
        assert!(
            after_ids.contains(&ev.event_id),
            "{} vanished on re-import",
            ev.provider_event_name
        );
        let same = after
            .events
            .iter()
            .find(|e| e.event_id == ev.event_id)
            .unwrap();
        assert_eq!(
            zeroed_for_golden(ev.clone()),
            zeroed_for_golden(same.clone())
        );
    }
}

// ---------------------------------------------------------------------------
// subagent_sidechain
// ---------------------------------------------------------------------------

#[test]
fn sidechain_entries_are_attributed_to_the_subagent() {
    let import = parse("subagent_sidechain", CaptureMode::LocalSemantic, true);
    assert_eq!(
        kinds(&import),
        vec![
            SubagentStarted,
            ToolCallStarted,
            ToolCallFinished,
            AgentMessage,
            SubagentStopped
        ]
    );
    assert_eq!(import.stats.subagent_entries, 4);
    assert_eq!(import.stats.prompts, 0);
    assert_eq!(import.stats.turns, 0);
    let session = import.events[0].session_id.to_string();
    let expected_agent = AgentId::derive(&["subagent", &session, "a1b2c3d4"]);
    let parent_agent = AgentId::derive(&["session", &session]);
    for ev in &import.events {
        assert_eq!(attr(ev, "is_sidechain"), &Value::Bool(true));
        assert_eq!(attr(ev, "is_subagent"), &Value::Bool(true));
        assert_eq!(ev.agent.provider_agent_id.as_deref(), Some("a1b2c3d4"));
        assert_eq!(ev.agent.agent_type.as_deref(), Some("Explore"));
        assert_eq!(ev.agent.agent_id, expected_agent);
        assert_eq!(ev.agent.parent_agent_id, Some(parent_agent));
        assert!(ev.attrs.get("turn_index_hint").is_none());
        assert!(ev.provider_turn_id.is_none());
    }
    let start = &import.events[0];
    assert_eq!(start.provider_event_name, "transcript:subagent_start");
    assert_eq!(
        start.observed_at,
        Timestamp::parse("2026-08-20T09:00:20.000Z").unwrap()
    );
    assert!(
        start
            .content
            .as_ref()
            .unwrap()
            .prompt
            .as_deref()
            .unwrap()
            .starts_with("CANARY_SUBAGENT_TASK")
    );
    assert_eq!(provider_attr(start, "parent_tool_use_id"), "toolu_0003");
    let stop = import.events.last().unwrap();
    assert_eq!(stop.provider_event_name, "transcript:subagent_stop");
    assert_eq!(
        stop.observed_at,
        Timestamp::parse("2026-08-20T09:00:24.000Z").unwrap()
    );
    let grep = &import.events[1];
    assert_eq!(grep.tool.as_ref().unwrap().category, ToolCategory::Search);
    assert_eq!(
        grep.paths.iter().map(|p| p.display()).collect::<Vec<_>>(),
        vec!["src"]
    );
}

// ---------------------------------------------------------------------------
// compaction_summary
// ---------------------------------------------------------------------------

#[test]
fn compaction_markers_become_compaction_finished() {
    let import = parse("compaction_summary", CaptureMode::LocalSemantic, true);
    assert_eq!(
        kinds(&import),
        vec![
            SessionStarted,
            CompactionFinished,
            PromptSubmitted,
            CompactionFinished,
            AgentMessage,
            TurnStopped,
            PromptSubmitted,
            AgentMessage,
            TurnStopped,
        ]
    );
    assert!(import.warnings.is_empty(), "{:?}", import.warnings);
    let summary = &import.events[1];
    assert_eq!(summary.provider_event_name, "transcript:summary");
    assert_eq!(attr(summary, "trigger"), "transcript_summary");
    assert_eq!(
        summary
            .content
            .as_ref()
            .unwrap()
            .extra
            .get("summary")
            .and_then(Value::as_str)
            .map(|s| s.starts_with("CANARY_OLD_SUMMARY")),
        Some(true)
    );
    // No timestamp on old summary entries: falls back to the capture time.
    assert_eq!(summary.observed_at, CAPTURED_AT);
    let boundary = &import.events[3];
    assert_eq!(
        boundary.provider_event_name,
        "transcript:system:compact_boundary"
    );
    assert_eq!(attr(boundary, "trigger"), "auto");
    assert_eq!(provider_attr(boundary, "pre_tokens"), 150000);
    assert_eq!(provider_attr(boundary, "post_tokens"), 12000);
    assert_eq!(
        boundary.observed_at,
        Timestamp::parse("2026-08-21T10:05:00.000Z").unwrap()
    );
    // The continuation summary is skipped, not a prompt.
    assert_eq!(import.stats.prompts, 2);
    assert_eq!(import.stats.skipped_entries, 1);
    // Old summary entries carry no gitBranch; every conversation entry does.
    assert!(
        import
            .events
            .iter()
            .enumerate()
            .all(|(i, e)| i == 1 || e.project.branch.as_deref() == Some("feature/compaction"))
    );
    assert_eq!(summary.project.branch, None);
}

// ---------------------------------------------------------------------------
// interrupted_turn
// ---------------------------------------------------------------------------

#[test]
fn interrupted_turns_are_cancelled_stops() {
    let import = parse("interrupted_turn", CaptureMode::LocalSemantic, true);
    assert_eq!(
        kinds(&import),
        vec![
            SessionStarted,
            PromptSubmitted,
            ToolCallStarted,
            ToolCallFailed,
            TurnStopped,
            PromptSubmitted,
            AgentMessage,
            TurnStopped,
            PromptSubmitted,
            AgentMessage,
            TurnStopped,
        ]
    );
    assert_eq!(import.stats.turns, 3);
    assert_eq!(import.stats.tool_failures, 1);
    let rejected = &import.events[3];
    let o = rejected.outcome.as_ref().unwrap();
    assert_eq!(
        (o.status, o.class.as_deref()),
        (OutcomeStatus::Denied, Some("user_rejected"))
    );
    assert_eq!(rejected.tool.as_ref().unwrap().name, "Bash");
    let stop_tool = &import.events[4];
    assert_eq!(stop_tool.provider_event_name, "transcript:user:interrupted");
    let o = stop_tool.outcome.as_ref().unwrap();
    assert_eq!(
        (o.status, o.class.as_deref()),
        (OutcomeStatus::Cancelled, Some("interrupted"))
    );
    assert_eq!(attr(stop_tool, "reason"), "user_interrupt");
    assert_eq!(provider_attr(stop_tool, "interrupt_kind"), "tool_use");
    // A partial message interrupted mid-stream is kept as a message, and the
    // interruption (not a synthesised turn end) closes the turn.
    let partial = &import.events[6];
    assert!(
        partial
            .content
            .as_ref()
            .unwrap()
            .message
            .as_deref()
            .unwrap()
            .starts_with("CANARY_PARTIAL_MESSAGE")
    );
    assert!(
        partial
            .attrs
            .get("provider")
            .and_then(|p| p.get("stop_reason"))
            .is_none()
    );
    let stop_turn = &import.events[7];
    assert_eq!(provider_attr(stop_turn, "interrupt_kind"), "turn");
    assert_eq!(
        stop_turn.observed_at,
        Timestamp::parse("2026-08-22T11:00:22.000Z").unwrap()
    );
    assert_eq!(import.events[10].provider_event_name, "transcript:turn_end");
}

// ---------------------------------------------------------------------------
// mixed_edge_cases
// ---------------------------------------------------------------------------

#[test]
fn unknown_entries_malformed_lines_and_in_flight_calls() {
    let import = parse("mixed_edge_cases", CaptureMode::LocalSemantic, true);
    assert_eq!(
        kinds(&import),
        vec![
            SessionStarted,
            PromptSubmitted,
            Unknown,
            Unknown,
            SubagentStarted,
            ToolCallStarted,
            ToolCallFinished,
            Notification,
            AgentMessage,
            ToolCallStarted,
            SubagentStopped,
        ]
    );
    let s = &import.stats;
    assert_eq!(
        (
            s.malformed_lines,
            s.unknown_entries,
            s.tool_calls,
            s.subagent_entries
        ),
        (2, 2, 2, 4)
    );
    assert_eq!(
        s.turns, 0,
        "a tool call left in flight synthesises no turn end"
    );
    assert_eq!(import.warnings.len(), 3, "{:?}", import.warnings);
    assert!(import.warnings[0].starts_with("line 2:"));
    assert!(import.warnings[1].starts_with("line 4:"));
    assert!(import.warnings[2].starts_with("line 5:"));

    let future = &import.events[2];
    assert_eq!(future.provider_event_name, "transcript:future-thing");
    assert_eq!(attr(future, "transcript_entry_type"), "future-thing");
    assert!(future.content.is_none());
    let untyped = &import.events[3];
    assert_eq!(untyped.provider_event_name, "transcript:untyped");
    assert!(!serde_json::to_string(untyped).unwrap().contains(CANARY));

    // Old-style inline sidechain without an agentId: one synthetic agent
    // per contiguous run, attributed to the session's subagent namespace.
    let inline_start = &import.events[4];
    assert_eq!(
        inline_start.agent.provider_agent_id.as_deref(),
        Some("sidechain:e0000004-0000-4000-8000-000000000000")
    );
    assert_eq!(inline_start.agent.agent_type, None);
    assert!(
        inline_start
            .content
            .as_ref()
            .unwrap()
            .prompt
            .as_deref()
            .unwrap()
            .starts_with("CANARY_INLINE_TASK")
    );
    // `system/api_error` is a retried failure of the request, not a turn end:
    // a notification carrying the numbers, the text only as content.
    let api_error = &import.events[7];
    assert_eq!(api_error.kind, Notification);
    assert_eq!(api_error.provider_event_name, "transcript:system:api_error");
    assert_eq!(attr(api_error, "notification_type"), "api_error");
    assert!(
        api_error
            .content
            .as_ref()
            .unwrap()
            .message
            .as_deref()
            .unwrap()
            .starts_with("CANARY_API_ERROR")
    );
    assert_eq!(import.stats.api_errors, 1);
    let inline_msg = &import.events[8];
    assert_eq!(inline_msg.kind, AgentMessage);
    assert_eq!(attr(inline_msg, "is_sidechain"), &Value::Bool(true));
    let main_call = &import.events[9];
    assert!(main_call.attrs.get("is_sidechain").is_none());
    assert_eq!(
        main_call.tool.as_ref().unwrap().call_id.as_deref(),
        Some("toolu_m003")
    );
    assert_eq!(attr(main_call, "git_subcommand"), "status");
    let inline_stop = &import.events[10];
    assert_eq!(
        inline_stop.observed_at,
        Timestamp::parse("2026-08-23T12:00:04.000Z").unwrap()
    );
    // Provider versions are per entry.
    assert_eq!(inline_msg.provider_version.as_deref(), Some("1.0.83"));
    assert_eq!(main_call.provider_version.as_deref(), Some("2.1.190"));
}

#[test]
fn session_id_falls_back_to_the_hint() {
    let stripped: Vec<String> = lines("basic_turn")
        .into_iter()
        .map(|l| match serde_json::from_str::<Value>(&l) {
            Ok(Value::Object(mut map)) => {
                map.remove("sessionId");
                Value::Object(map).to_string()
            }
            _ => l,
        })
        .collect();
    let import = parse_lines("basic_turn", stripped);
    assert_eq!(
        import.provider_session_id.as_deref(),
        Some("basic_turn-stem")
    );
    assert!(
        import
            .warnings
            .iter()
            .any(|w| w.contains("using the file name"))
    );
    assert_eq!(import.events.len(), 12);
    assert!(
        import
            .events
            .iter()
            .all(|e| e.provider_session_id == "basic_turn-stem")
    );

    let none = parse_claude_transcript(
        vec![r#"{"type":"user","message":{"role":"user","content":"hi"},"uuid":"u1","timestamp":"2026-08-20T09:00:00.000Z"}"#.to_string()].into_iter(),
        &context(CaptureMode::LocalSemantic),
        &TranscriptOptions::default(),
    );
    assert_eq!(none.provider_session_id, None);
    assert_eq!(
        none.events.iter().map(|e| e.kind).collect::<Vec<_>>(),
        vec![SessionStarted, PromptSubmitted]
    );
}

#[test]
fn nothing_panics_on_garbage() {
    let garbage = vec![
        String::new(),
        "null".into(),
        "42".into(),
        r#"{"type":"user"}"#.into(),
        r#"{"type":"assistant","message":{"content":"not-an-array"}}"#.into(),
        r#"{"type":"user","message":{"content":[{"type":"tool_result"}]}}"#.into(),
        r#"{"type":"system"}"#.into(),
        r#"{"type":"summary"}"#.into(),
        r#"{"type":"user","message":{"content":[{"type":"text"}]},"isSidechain":true}"#.into(),
    ];
    let import = parse_claude_transcript(
        garbage.into_iter(),
        &context(CaptureMode::MetadataOnly),
        &TranscriptOptions::default(),
    );
    assert_eq!(import.stats.malformed_lines, 2);
    assert!(import.events.iter().all(|e| e.content.is_none()));
}

// ---------------------------------------------------------------------------
// Dedup fidelity: ids shared with the hooks, queued prompts, narration, usage
// ---------------------------------------------------------------------------

const Q_SESSION: &str = "55555555-5555-4555-8555-555555555555";

fn by_name<'a>(import: &'a TranscriptImport, name: &str) -> Vec<&'a Event> {
    import
        .events
        .iter()
        .filter(|e| e.provider_event_name == name)
        .collect()
}

fn prompt_text(ev: &Event) -> &str {
    ev.content
        .as_ref()
        .and_then(|c| c.prompt.as_deref())
        .unwrap_or("")
}

#[test]
fn tool_events_carry_the_natural_id_and_report_the_old_one() {
    let import = parse("basic_turn", CaptureMode::LocalSemantic, true);
    let session = import.provider_session_id.clone().unwrap();
    assert_eq!(import.legacy_ids.len(), import.events.len());
    let mut natural = 0;
    for (ev, legacy) in import.events.iter().zip(&import.legacy_ids) {
        match ev.kind {
            ToolCallStarted | ToolCallFinished | ToolCallFailed => {
                natural += 1;
                let call = ev.tool.as_ref().unwrap().call_id.clone().unwrap();
                assert_eq!(
                    ev.event_id,
                    derive_event_id(
                        &Provider::ClaudeCode,
                        &session,
                        ev.kind,
                        &tool_call_key(&call)
                    ),
                    "{}: the id every channel derives for this call",
                    ev.provider_event_name
                );
                assert!(legacy.is_some(), "{}: legacy id", ev.provider_event_name);
                assert_ne!(Some(ev.event_id), *legacy);
            }
            _ => assert!(
                legacy.is_none(),
                "{}: only tool events change id",
                ev.provider_event_name
            ),
        }
    }
    assert_eq!(natural, 4);

    // The old id is exactly what earlier releases stored: `(session, entry
    // uuid, block)` under the `transcript` prefix. First tool_use block of
    // entry a0000004 (toolu_0001), first tool_result of a0000005.
    let start = import
        .events
        .iter()
        .zip(&import.legacy_ids)
        .find(|(e, _)| e.kind == ToolCallStarted)
        .unwrap();
    assert_eq!(
        *start.1,
        Some(EventId::derive(&[
            "transcript",
            &session,
            "a0000004-0000-4000-8000-000000000000",
            "block:0"
        ]))
    );
    let end = import
        .events
        .iter()
        .zip(&import.legacy_ids)
        .find(|(e, _)| e.kind == ToolCallFinished)
        .unwrap();
    assert_eq!(
        *end.1,
        Some(EventId::derive(&[
            "transcript",
            &session,
            "a0000005-0000-4000-8000-000000000000",
            "block:0"
        ]))
    );
    // Everything else keeps the id it always had.
    let prompt = &import.events[1];
    assert_eq!(
        prompt.event_id,
        EventId::derive(&[
            "transcript",
            &session,
            "a0000001-0000-4000-8000-000000000000",
            "entry"
        ])
    );
}

#[test]
fn a_replayed_tool_line_is_one_event() {
    let mut all = lines("basic_turn");
    // Repeat the whole first tool exchange (call and result), as a resumed or
    // compacted transcript can.
    let replay: Vec<String> = all[8..10].to_vec();
    let at = all.len();
    all.splice(at..at, replay);
    let import = parse_lines("basic_turn", all);
    let calls = by_name(&import, "transcript:assistant:tool_use");
    let results = by_name(&import, "transcript:user:tool_result");
    assert_eq!((calls.len(), results.len()), (2, 2));
    assert_eq!(import.stats.tool_calls, 2);
    let ids: HashSet<EventId> = import.events.iter().map(|e| e.event_id).collect();
    assert_eq!(ids.len(), import.events.len());
}

#[test]
fn queued_commands_are_prompts_and_task_notifications_are_not() {
    let import = parse("queued_and_usage", CaptureMode::LocalSemantic, true);
    let prompts = by_name(&import, "transcript:user");
    let texts: Vec<&str> = prompts.iter().map(|e| prompt_text(e)).collect();
    assert_eq!(
        texts,
        vec![
            "CANARY_PROMPT_Q start the refactor",
            "CANARY_QUEUED_ONE also keep the API stable",
            "CANARY_QUEUED_TWO check the docs too",
            "CANARY_PROMPT_Q2 now add the tests",
        ],
        "the task notification and the edited-file attachment are not prompts"
    );
    assert_eq!(import.stats.prompts, 4);
    assert_eq!(import.stats.queued_prompts, 2);
    let queued = prompts[1];
    assert_eq!(queued.kind, PromptSubmitted);
    assert_eq!(provider_attr(queued, "prompt_source"), "queued_command");
    assert_eq!(provider_attr(queued, "prompt_kind"), "text");
    assert_eq!(attr(queued, "transcript_entry_type"), "attachment");
    assert_eq!(attr(queued, "reconstructed"), &Value::Bool(true));
    assert_eq!(
        queued.observed_at,
        Timestamp::parse("2026-08-24T10:00:02.000Z").unwrap()
    );
    assert_eq!(
        provider_attr(prompts[2], "image_count"),
        1,
        "a block-list prompt keeps its text and counts its images"
    );
    // Typed prompts keep their provider id; attachments carry none.
    assert_eq!(
        prompts[0].provider_turn_id.as_deref(),
        Some("p0000041-0000-4000-8000-000000000000")
    );
    // Each queued prompt opens a turn of its own, like the hook does.
    assert_eq!(attr(prompts[1], "turn_index_hint"), 2);
    assert_eq!(attr(prompts[2], "turn_index_hint"), 3);
    assert_eq!(attr(prompts[3], "turn_index_hint"), 4);

    // Content policy: nothing of a queued prompt survives metadata-only.
    let blind = parse("queued_and_usage", CaptureMode::MetadataOnly, false);
    let queued = by_name(&blind, "transcript:user");
    assert_eq!(queued.len(), 4);
    assert!(
        queued
            .iter()
            .all(|e| e.content.is_none() && attr(e, "prompt_chars").is_u64())
    );
    assert!(
        !serde_json::to_string(&blind.events)
            .unwrap()
            .contains(CANARY)
    );
}

#[test]
fn a_prompt_queued_mid_turn_does_not_end_the_running_turn() {
    let import = parse("queued_and_usage", CaptureMode::LocalSemantic, true);
    let kinds = kinds(&import);
    // prompt, narration, tool, <queued prompt>, result: the narration before
    // the queued prompt is interim and no turn end sits between them.
    let first_queued = import
        .events
        .iter()
        .position(|e| prompt_text(e).starts_with("CANARY_QUEUED_ONE"))
        .unwrap();
    assert!(!kinds[..first_queued].contains(&TurnStopped), "{kinds:?}");
}

#[test]
fn narration_is_kept_and_marked_interim() {
    let import = parse("queued_and_usage", CaptureMode::LocalSemantic, true);
    let messages = by_name(&import, "transcript:assistant:text");
    assert_eq!(messages.len(), 3);
    let narration = messages[0];
    assert!(
        narration
            .content
            .as_ref()
            .unwrap()
            .message
            .as_deref()
            .unwrap()
            .starts_with("CANARY_NARRATION_Q")
    );
    assert_eq!(provider_attr(narration, "interim"), &Value::Bool(true));
    let final_message = messages[1];
    assert_eq!(provider_attr(final_message, "interim"), &Value::Null);
    assert_eq!(import.stats.interim_messages, 1);
    // Narration is metadata-clean: only `content` holds the words.
    let blind = parse("queued_and_usage", CaptureMode::MetadataOnly, true);
    let blind_narration = by_name(&blind, "transcript:assistant:text")[0];
    assert!(blind_narration.content.is_none());
    assert_eq!(provider_attr(blind_narration, "message_chars"), 40);
}

#[test]
fn turn_ends_are_synthesised_only_for_turns_that_ended() {
    let import = parse("queued_and_usage", CaptureMode::LocalSemantic, true);
    let stops = by_name(&import, "transcript:turn_end");
    assert_eq!(stops.len(), 1, "one completed turn, one still running");
    assert_eq!(stops[0].duration_ms, Some(9300), "from turn_duration");
    // The file ends inside the second prompt's turn: its last message is
    // there, a turn end for it is not.
    let last = import.events.last().unwrap();
    assert_eq!(last.kind, AgentMessage);
    assert!(
        last.content
            .as_ref()
            .unwrap()
            .message
            .as_deref()
            .unwrap()
            .starts_with("CANARY_MIDTURN_Q")
    );
    assert_eq!(import.stats.turns, 1);

    // A `turn_duration` entry ends the turn even when the message never got
    // a stop reason (older builds wrote `null` throughout).
    let mut cut = lines("queued_and_usage");
    let end = cut
        .iter()
        .position(|l| l.contains("CANARY_PROMPT_Q2"))
        .unwrap();
    cut.truncate(end + 1);
    cut.push(
        r#"{"type":"assistant","sessionId":"55555555-5555-4555-8555-555555555555","uuid":"f0000020-0000-4000-8000-000000000000","timestamp":"2026-08-24T10:01:02.000Z","message":{"id":"msg_q005","role":"assistant","content":[{"type":"text","text":"CANARY_NULL_STOP done."}],"stop_reason":null}}"#.into(),
    );
    let open = parse_lines("queued_and_usage", cut.clone());
    assert!(
        !open
            .events
            .iter()
            .any(|e| e.kind == TurnStopped && attr(e, "turn_index_hint") == 4),
        "no turn_duration, no stop_reason: still open"
    );
    cut.push(
        r#"{"type":"system","subtype":"turn_duration","durationMs":2000,"sessionId":"55555555-5555-4555-8555-555555555555","uuid":"f0000021-0000-4000-8000-000000000000","timestamp":"2026-08-24T10:01:02.500Z"}"#.into(),
    );
    let closed = parse_lines("queued_and_usage", cut);
    let stop = closed.events.last().unwrap();
    assert_eq!(stop.kind, TurnStopped);
    assert_eq!(stop.duration_ms, Some(2000));
}

#[test]
fn the_end_of_the_file_closes_a_turn_whose_last_message_says_it_ended() {
    // `basic_turn` ends right after a message with `stop_reason: end_turn`
    // and no `turn_duration`: that turn is over.
    let import = parse("basic_turn", CaptureMode::LocalSemantic, true);
    assert_eq!(import.events.last().unwrap().kind, TurnStopped);
    // The same file cut inside the last message's turn, with the message
    // still streaming (`stop_reason: null`), is not.
    let mut all = lines("basic_turn");
    let last = all.pop().unwrap();
    let streaming = last.replace(r#""stop_reason":"end_turn""#, r#""stop_reason":null"#);
    assert_ne!(last, streaming);
    all.push(streaming);
    let open = parse_lines("basic_turn", all);
    assert_eq!(open.events.last().unwrap().kind, AgentMessage);
    assert_eq!(open.stats.turns, 1);
}

#[test]
fn usage_numbers_land_once_on_the_first_event_of_each_message() {
    let import = parse("queued_and_usage", CaptureMode::LocalSemantic, true);
    // msg_q001: narration line + tool_use line; the newest line's numbers
    // (output_tokens 31) are on the message's first event, the narration.
    let narration = by_name(&import, "transcript:assistant:text")[0];
    for (key, want) in [
        ("input_tokens", 100u64),
        ("output_tokens", 31),
        ("cache_creation_input_tokens", 2000),
        ("cache_read_input_tokens", 9000),
        ("web_search_requests", 0),
        ("web_fetch_requests", 0),
    ] {
        assert_eq!(provider_attr(narration, key), want, "{key}");
    }
    let bash = by_name(&import, "transcript:assistant:tool_use")[0];
    assert_eq!(provider_attr(bash, "input_tokens"), &Value::Null);
    // msg_q002 has no text: its numbers sit on its first tool event.
    let search = by_name(&import, "transcript:assistant:server_tool_use")[0];
    assert_eq!(provider_attr(search, "web_search_requests"), 1);
    assert_eq!(provider_attr(search, "web_fetch_requests"), 1);
    assert_eq!(provider_attr(search, "cache_read_input_tokens"), 11000);
    let fetch = by_name(&import, "transcript:assistant:server_tool_use")[1];
    assert_eq!(provider_attr(fetch, "input_tokens"), &Value::Null);
    // The final message has its own.
    let final_message = by_name(&import, "transcript:assistant:text")[1];
    assert_eq!(provider_attr(final_message, "output_tokens"), 18);
    assert_eq!(provider_attr(final_message, "input_tokens"), 300);
    // Numbers only, never text, wherever they are.
    for ev in &import.events {
        if let Some(provider) = ev.attrs.get("provider") {
            for (k, v) in provider.as_object().unwrap() {
                if k.ends_with("_tokens") || k.ends_with("_requests") {
                    assert!(v.is_u64(), "{k}");
                }
            }
        }
    }
}

#[test]
fn server_tools_are_tool_calls_that_close_with_their_result_block() {
    let import = parse("queued_and_usage", CaptureMode::LocalSemantic, true);
    let started = by_name(&import, "transcript:assistant:server_tool_use");
    assert_eq!(started.len(), 2);
    let t = started[0].tool.as_ref().unwrap();
    assert_eq!(
        (t.name.as_str(), t.category, t.call_id.as_deref()),
        ("web_search", ToolCategory::Web, Some("srvtoolu_q001"))
    );
    assert_eq!(provider_attr(started[0], "server_tool"), &Value::Bool(true));
    let ends = by_name(&import, "transcript:assistant:server_tool_result");
    assert_eq!(ends.len(), 2);
    assert_eq!(ends[0].kind, ToolCallFinished);
    assert_eq!(provider_attr(ends[0], "result_count"), 2);
    assert_eq!(ends[0].tool.as_ref().unwrap().name, "web_search");
    assert_eq!(ends[1].kind, ToolCallFailed);
    let o = ends[1].outcome.as_ref().unwrap();
    assert_eq!(
        (o.status, o.class.as_deref()),
        (OutcomeStatus::Failure, Some("url_not_accessible"))
    );
    // The result's pages are never kept.
    let serialised = serde_json::to_string(&import.events).unwrap();
    assert!(!serialised.contains("CANARY_SEARCH_TITLE"));
    assert!(!serialised.contains("example.com/a"));
    // Same call id on both sides: the projection pairs them.
    assert_eq!(
        started[1].tool.as_ref().unwrap().call_id,
        ends[1].tool.as_ref().unwrap().call_id
    );
    assert_eq!(import.stats.tool_failures, 1);
}

#[test]
fn api_errors_keep_their_numbers() {
    let import = parse("queued_and_usage", CaptureMode::LocalSemantic, true);
    let errors = by_name(&import, "transcript:system:api_error");
    assert_eq!(errors.len(), 1);
    let ev = errors[0];
    assert_eq!(ev.kind, Notification);
    assert_eq!(attr(ev, "notification_type"), "api_error");
    assert_eq!(attr(ev, "error_class"), "overloaded_error");
    assert_eq!(provider_attr(ev, "http_status"), 529);
    assert_eq!(provider_attr(ev, "retry_attempt"), 2);
    assert_eq!(provider_attr(ev, "max_retries"), 10);
    assert_eq!(provider_attr(ev, "retry_in_ms"), 1000);
    assert!(
        ev.content
            .as_ref()
            .unwrap()
            .message
            .as_deref()
            .unwrap()
            .starts_with("CANARY_API_Q")
    );
}

#[test]
fn cost_state_keeps_its_numbers_or_is_skipped_never_an_empty_shell() {
    let import = parse("queued_and_usage", CaptureMode::LocalSemantic, true);
    let costs = by_name(&import, "transcript:cost-state");
    assert_eq!(costs.len(), 1, "the entry without numbers is skipped");
    let ev = costs[0];
    assert_eq!(ev.kind, Unknown);
    assert_eq!(provider_attr(ev, "totalInputTokens"), 1200);
    assert_eq!(provider_attr(ev, "totalLinesAdded"), 10);
    assert_eq!(provider_attr(ev, "totalCostUSD").as_f64(), Some(0.4213));
    assert!(ev.content.is_none());
    assert_eq!(
        import.stats.unknown_entries, 0,
        "cost-state is a known type"
    );
    let serialised = serde_json::to_string(&import.events).unwrap();
    assert!(!serialised.contains("CANARY_NESTED_COST"));
    assert!(!serialised.contains("CANARY_EMPTY_COST"));
    // Every `unknown` the parser still emits carries at least its type name
    // and nothing is an anonymous shell.
    assert!(import.events.iter().filter(|e| e.kind == Unknown).all(|e| {
        e.provider_event_name.starts_with("transcript:") && e.provider_event_name.len() > 11
    }));
}

// ---------------------------------------------------------------------------
// Golden envelopes
// ---------------------------------------------------------------------------

#[test]
fn golden_envelopes_match() {
    let update = std::env::var("UPDATE_GOLDEN").is_ok_and(|v| v == "1");
    let mut mismatches = Vec::new();
    for name in FIXTURES {
        let import = parse(name, CaptureMode::LocalSemantic, true);
        let actual: Vec<Value> = import
            .events
            .into_iter()
            .map(|e| serde_json::to_value(zeroed_for_golden(e)).expect("serialise event"))
            .collect();
        let actual = Value::Array(actual);
        let golden_path = fixtures_dir().join(format!("{name}.golden.json"));
        let rendered = format!("{}\n", serde_json::to_string_pretty(&actual).unwrap());
        if update || !golden_path.exists() {
            fs::write(&golden_path, &rendered).expect("write golden");
            continue;
        }
        let golden: Value =
            serde_json::from_str(&fs::read_to_string(&golden_path).expect("read golden"))
                .unwrap_or_else(|e| panic!("{}: invalid JSON: {e}", golden_path.display()));
        if golden != actual {
            mismatches.push(format!(
                "{}\n--- expected ---\n{}\n--- actual ---\n{rendered}",
                golden_path.display(),
                serde_json::to_string_pretty(&golden).unwrap()
            ));
        }
        for item in golden.as_array().expect("golden is an array") {
            let back: Event = serde_json::from_value(item.clone()).expect("golden deserialises");
            assert!(
                back.unknown.is_empty(),
                "{}: unknown fields",
                golden_path.display()
            );
        }
    }
    assert!(
        mismatches.is_empty(),
        "{} golden mismatch(es); run with UPDATE_GOLDEN=1 to regenerate:\n\n{}",
        mismatches.len(),
        mismatches.join("\n\n")
    );
}

// ---------------------------------------------------------------------------
// Privacy
// ---------------------------------------------------------------------------

#[test]
fn metadata_only_output_carries_no_content() {
    for name in FIXTURES {
        for (mode, include) in [
            (CaptureMode::LocalSemantic, false),
            (CaptureMode::MetadataOnly, true),
            (CaptureMode::MetadataOnly, false),
        ] {
            let import = parse(name, mode, include);
            assert!(!import.events.is_empty());
            for ev in &import.events {
                assert!(
                    ev.content.is_none(),
                    "{name}: content present ({mode}, include_content={include})"
                );
                assert!(ev.raw.is_none(), "{name}: raw present");
            }
            let serialised = serde_json::to_string(&import.events).unwrap();
            assert!(
                !serialised.contains(CANARY),
                "{name} ({mode}, include_content={include}): content leaked:\n{serialised}"
            );
            assert!(
                !serialised.contains("must-not-appear"),
                "{name}: hook version leaked"
            );
            assert!(
                !serialised.contains(".jsonl"),
                "{name}: transcript path leaked"
            );
        }
    }
}

#[test]
fn content_only_ever_lives_in_content() {
    for name in FIXTURES {
        let import = parse(name, CaptureMode::LocalSemantic, true);
        let mut saw_content = false;
        for ev in &import.events {
            let mut stripped = ev.clone();
            saw_content |= stripped.content.is_some();
            stripped.content = None;
            let serialised = serde_json::to_string(&stripped).unwrap();
            assert!(
                !serialised.contains(CANARY),
                "{name}/{}: content outside `content`:\n{serialised}",
                ev.provider_event_name
            );
            if let Some(provider) = ev.attrs.get("provider") {
                for (k, v) in provider.as_object().unwrap() {
                    assert!(
                        v.is_number()
                            || v.is_boolean()
                            || v.as_str().is_some_and(|s| s.len() <= 64),
                        "{name}: provider attr `{k}` is not a short scalar"
                    );
                }
            }
        }
        assert!(
            saw_content,
            "{name}: expected some content in local_semantic mode"
        );
    }
}
