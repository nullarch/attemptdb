//! Fixture-driven contract tests for the Codex rollout parser.
//!
//! Fixtures live in `fixtures/transcripts/codex/<name>.jsonl`; each has a
//! golden envelope list next to it (`<name>.golden.json`) generated when
//! missing and regenerated with `UPDATE_GOLDEN=1`. Every content-bearing
//! string in the fixtures carries a `CANARY_` marker so leaks into metadata
//! are caught by a plain substring search.

use super::codex::{
    CodexRolloutImport, CodexRolloutOptions, parse_codex_rollout_to_vec, peek_rollout_meta,
};
use crate::CaptureContext;
use crate::common::ALLOWED_ATTR_KEYS;
use attemptdb_core::event::Provider;
use attemptdb_core::{
    CaptureMode, DeviceId, Event, EventId, EventKind, OutcomeStatus, ProjectRef, SessionId,
    Timestamp, ToolCategory,
};
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};

const PROJECT_ROOT: &str = "/home/dev/example/project";
const PROJECT_REMOTE: &str = "git@github.com:example/project.git";
const CAPTURED_AT: Timestamp = Timestamp::from_micros(1_787_904_000_000_000);
const CANARY: &str = "CANARY_";

const FIXTURES: &[&str] = &["modern_turn", "classic_turn", "subagent_thread"];

use EventKind::*;

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/transcripts/codex")
}

fn device_id() -> DeviceId {
    DeviceId::derive(&["codex-transcript-tests"])
}

fn context(mode: CaptureMode) -> CaptureContext {
    let device_id = device_id();
    CaptureContext {
        device_id,
        capture_mode: mode,
        project: ProjectRef::derive(PROJECT_ROOT, Some(PROJECT_REMOTE), &device_id),
        captured_at: CAPTURED_AT,
        provider_version: Some("must-not-appear-provider-version".into()),
        hook_version: Some("must-not-appear".into()),
    }
}

fn options_for(name: &str, include_content: bool) -> CodexRolloutOptions {
    CodexRolloutOptions {
        include_content,
        session_id_hint: Some(format!("{name}-stem")),
        ..CodexRolloutOptions::default()
    }
}

fn bytes(name: &str) -> Vec<u8> {
    let path = fixtures_dir().join(format!("{name}.jsonl"));
    fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn parse(name: &str, mode: CaptureMode, include_content: bool) -> CodexRolloutImport {
    parse_codex_rollout_to_vec(
        Cursor::new(bytes(name)),
        &context(mode),
        &options_for(name, include_content),
    )
}

fn parse_text(text: &str) -> CodexRolloutImport {
    parse_codex_rollout_to_vec(
        Cursor::new(text.as_bytes().to_vec()),
        &context(CaptureMode::LocalSemantic),
        &options_for("inline", true),
    )
}

fn kinds(import: &CodexRolloutImport) -> Vec<EventKind> {
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
// Provenance shared by every event
// ---------------------------------------------------------------------------

/// The provider session id each fixture's events are attributed to.
fn expected_session(name: &str) -> &'static str {
    match name {
        "modern_turn" => "22222222-2222-4222-8222-222222222222",
        "classic_turn" => "33333333-3333-4333-8333-333333333333",
        // A subagent thread belongs to its parent's session.
        "subagent_thread" => "44444444-4444-4444-8444-444444444444",
        other => panic!("unknown fixture {other}"),
    }
}

#[test]
fn every_event_is_marked_reconstructed() {
    for name in FIXTURES {
        let import = parse(name, CaptureMode::LocalSemantic, true);
        assert!(!import.events.is_empty(), "{name}: no events");
        let session = expected_session(name);
        assert_eq!(
            import.summary.provider_session_id.as_deref(),
            Some(session),
            "{name}"
        );
        let expected_session_id = SessionId::derive(&[Provider::Codex.as_str(), session]);
        for ev in &import.events {
            let label = format!("{name}/{}", ev.provider_event_name);
            assert_eq!(ev.provider, Provider::Codex, "{label}");
            assert_eq!(ev.adapter_version, crate::ADAPTER_VERSION, "{label}");
            assert_eq!(ev.hook_version, None, "{label}: hook_version");
            assert_eq!(ev.raw, None, "{label}: raw");
            assert_eq!(ev.captured_at, CAPTURED_AT, "{label}: captured_at");
            assert_eq!(ev.provider_session_id, session, "{label}: session");
            assert_eq!(ev.session_id, expected_session_id, "{label}: derived");
            assert!(
                ev.provider_event_name.starts_with("transcript:"),
                "{label}: name"
            );
            assert_eq!(attr(ev, "reconstructed"), &Value::Bool(true), "{label}");
            assert_eq!(attr(ev, "reconstructed_from"), "codex_rollout", "{label}");
            assert_eq!(
                attr(ev, "transcript_present"),
                &Value::Bool(true),
                "{label}"
            );
            assert!(attr(ev, "transcript_entry_type").is_string(), "{label}");
            assert!(ev.attrs.get("hook_event_name").is_none(), "{label}");
            assert_ne!(ev.observed_at, Timestamp::from_micros(0), "{label}");
            assert_ne!(
                ev.provider_version.as_deref(),
                Some("must-not-appear-provider-version"),
                "{label}: the rollout's cli_version replaces the context's"
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
        assert_eq!(ids_a, ids_b, "{name}: ids depend on the rollout only");
        let unique: HashSet<EventId> = ids_a.iter().copied().collect();
        assert_eq!(unique.len(), ids_a.len(), "{name}: duplicate ids");
        assert!(ids_a.iter().all(|id| !id.is_nil()));
    }
}

#[test]
fn a_grown_file_only_adds_new_ids() {
    // Re-importing after the session continued: every earlier event keeps
    // its id (so the store skips it) and only the new lines add ids.
    let text = String::from_utf8(bytes("classic_turn")).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    let head = lines[..20].join("\n") + "\n";
    let first = parse_text(&head);
    let all = parse_text(&text);
    // The end-of-session event moves with the end of the file.
    let first_ids: HashSet<EventId> = first
        .events
        .iter()
        .filter(|e| e.kind != SessionEnded)
        .map(|e| e.event_id)
        .collect();
    let all_ids: HashSet<EventId> = all.events.iter().map(|e| e.event_id).collect();
    assert!(first_ids.is_subset(&all_ids), "earlier ids are stable");
    assert!(all_ids.len() > first_ids.len());
    let ends: Vec<&Event> = all
        .events
        .iter()
        .filter(|e| e.kind == SessionEnded)
        .collect();
    assert_eq!(ends.len(), 1);
    assert!(
        !first
            .events
            .iter()
            .any(|e| e.kind == SessionEnded && e.event_id == ends[0].event_id),
        "a later end of the session is a new event, not a replacement"
    );
}

// ---------------------------------------------------------------------------
// modern_turn (item style)
// ---------------------------------------------------------------------------

#[test]
fn modern_turn_kinds_and_order() {
    let import = parse("modern_turn", CaptureMode::LocalSemantic, true);
    assert_eq!(
        kinds(&import),
        vec![
            SessionStarted,
            PromptSubmitted,
            AgentMessage,
            ToolCallStarted, // exec (the code-mode runner)
            ToolCallStarted, // CommandExecution: cargo test (fails)
            ToolCallFailed,
            ToolCallStarted, // FileChange
            ToolCallFinished,
            ToolCallStarted, // CommandExecution: cargo test (passes)
            ToolCallFinished,
            ToolCallFinished, // exec output
            ToolCallStarted,  // MCP search
            ToolCallFinished,
            ToolCallStarted, // MCP fetch (fails)
            ToolCallFailed,
            ToolCallStarted, // ImageView
            ToolCallFinished,
            ToolCallStarted, // web search
            ToolCallFinished,
            ToolCallStarted, // image generation
            ToolCallFinished,
            AgentMessage,
            CompactionFinished,
            TurnStopped,
            PromptSubmitted,
            Unknown,
            TurnFailed,
            Notification,
            PromptSubmitted,
            TurnFailed,
            SessionEnded,
        ]
    );
    assert!(
        import.summary.warnings.is_empty(),
        "{:?}",
        import.summary.warnings
    );
    let s = &import.summary.stats;
    assert_eq!(
        (
            s.prompts,
            s.messages,
            s.tool_calls,
            s.tool_failures,
            s.turns
        ),
        (3, 2, 9, 2, 3)
    );
    assert_eq!(
        (
            s.unknown_entries,
            s.malformed_lines,
            s.oversized_lines,
            s.compactions
        ),
        (1, 0, 0, 1)
    );
    // Bookkeeping, mirrors and duplicate encodings: nothing of them is an event.
    assert_eq!(s.skipped_entries, 18);
    assert_eq!(import.summary.events, import.events.len());
}

#[test]
fn modern_turn_session_and_project() {
    let import = parse("modern_turn", CaptureMode::LocalSemantic, true);
    let start = &import.events[0];
    assert_eq!(start.provider_event_name, "transcript:session_meta");
    assert_eq!(attr(start, "source"), "transcript");
    assert_eq!(attr(start, "entrypoint"), "codex_tui");
    assert_eq!(provider_attr(start, "cli_source"), "cli");
    assert_eq!(
        start.observed_at,
        Timestamp::parse("2026-08-28T08:00:00.000Z").unwrap()
    );
    assert_eq!(start.provider_version.as_deref(), Some("0.154.0"));
    assert_eq!(attr(start, "cwd"), "~/example/project");
    assert_eq!(
        start.project.branch.as_deref(),
        Some("main"),
        "the rollout's git branch fills the branch the context lacks"
    );
    assert!(
        import
            .events
            .iter()
            .all(|e| e.agent.model.as_deref() == Some("gpt-5.5") || e.provider_turn_id.is_none())
    );
    let end = import.events.last().unwrap();
    assert_eq!(end.kind, SessionEnded);
    assert_eq!(provider_attr(end, "session_total_tokens"), 2550);
    assert_eq!(provider_attr(end, "session_output_tokens"), 450);
}

#[test]
fn project_branch_prefers_the_context() {
    let mut ctx = context(CaptureMode::LocalSemantic);
    ctx.project.branch = Some("from-ctx".into());
    let import = parse_codex_rollout_to_vec(
        Cursor::new(bytes("modern_turn")),
        &ctx,
        &options_for("modern_turn", true),
    );
    assert!(
        import
            .events
            .iter()
            .all(|e| e.project.branch.as_deref() == Some("from-ctx"))
    );
}

#[test]
fn prompts_pair_across_encodings_and_carry_facts() {
    let import = parse("modern_turn", CaptureMode::LocalSemantic, true);
    let prompts: Vec<&Event> = import
        .events
        .iter()
        .filter(|e| e.kind == PromptSubmitted)
        .collect();
    assert_eq!(prompts.len(), 3, "the response-item mirror is not a prompt");
    let first = prompts[0];
    assert!(
        first
            .content
            .as_ref()
            .unwrap()
            .prompt
            .as_deref()
            .unwrap()
            .starts_with("CANARY_PROMPT_ONE")
    );
    assert_eq!(attr(first, "prompt_has_question"), &Value::Bool(true));
    assert_eq!(attr(first, "permission_mode"), "on-request");
    assert_eq!(provider_attr(first, "prompt_kind"), "text");
    assert_eq!(first.provider_turn_id.as_deref(), Some("turn-0001"));
    assert_eq!(attr(first, "turn_index_hint"), 1);
    assert_eq!(attr(prompts[1], "turn_index_hint"), 2);
}

#[test]
fn agent_messages_keep_their_phase_and_pair_duplicates() {
    let import = parse("modern_turn", CaptureMode::LocalSemantic, true);
    let msgs: Vec<&Event> = import
        .events
        .iter()
        .filter(|e| e.kind == AgentMessage)
        .collect();
    assert_eq!(msgs.len(), 2);
    assert_eq!(provider_attr(msgs[0], "phase"), "commentary");
    assert_eq!(provider_attr(msgs[1], "phase"), "final_answer");
    assert!(
        msgs[1]
            .content
            .as_ref()
            .unwrap()
            .message
            .as_deref()
            .unwrap()
            .starts_with("CANARY_AGENT_FINAL")
    );
}

#[test]
fn item_style_tool_calls_pair_and_carry_facts() {
    let import = parse("modern_turn", CaptureMode::LocalSemantic, true);
    let ev = &import.events;
    // The failing cargo test: command, exit code and counts, never the output.
    let (start, end) = (&ev[4], &ev[5]);
    let t = start.tool.as_ref().unwrap();
    assert_eq!(
        (t.name.as_str(), t.category, t.call_id.as_deref()),
        ("exec_command", ToolCategory::Shell, Some("exec-aaa0001"))
    );
    assert_eq!(attr(start, "command_category"), "test");
    assert_eq!(provider_attr(start, "exec_source"), "unified_exec_startup");
    assert_eq!(end.kind, ToolCallFailed);
    let o = end.outcome.as_ref().unwrap();
    assert_eq!(
        (o.status, o.class.as_deref(), o.exit_code),
        (OutcomeStatus::Failure, Some("nonzero_exit"), Some(101))
    );
    assert_eq!(end.duration_ms, Some(2500));
    assert_eq!(attr(end, "tests_passed"), 11);
    assert_eq!(attr(end, "tests_failed"), 1);
    assert!(
        start.observed_at < end.observed_at,
        "start and end use the item's own times"
    );
    assert_eq!(
        end.observed_at.as_millis() - start.observed_at.as_millis(),
        2500
    );

    // The file change: both files, lines from the diff and the new file.
    let fc = &ev[6];
    assert_eq!(fc.tool.as_ref().unwrap().category, ToolCategory::FileEdit);
    assert_eq!(fc.paths.len(), 2);
    assert_eq!(attr(fc, "lines_added"), 4);
    assert_eq!(attr(fc, "lines_removed"), 1);
    assert_eq!(fc.paths[0].repo_relative.as_deref(), Some("src/lib.rs"));

    // MCP: a name in the shared `mcp__server__tool` vocabulary.
    let mcp = &ev[11];
    let t = mcp.tool.as_ref().unwrap();
    assert_eq!(
        (t.name.as_str(), t.category),
        ("mcp__docs__search", ToolCategory::Mcp)
    );
    let failed = &ev[14];
    assert_eq!(failed.kind, ToolCallFailed);
    assert_eq!(
        failed.outcome.as_ref().unwrap().class.as_deref(),
        Some("timeout")
    );

    // The code-mode runner and the inner command both exist.
    let exec_end = &ev[10];
    assert_eq!(exec_end.tool.as_ref().unwrap().name, "exec");
    // `exec` is Codex's code-mode runner; the shared tool vocabulary files it
    // under shell (it runs commands), like `exec_command`.
    assert_eq!(
        exec_end.tool.as_ref().unwrap().category,
        ToolCategory::Shell
    );
    assert_eq!(exec_end.duration_ms, Some(6400));
    assert!(exec_end.paths.is_empty());

    // View, web search, image generation.
    assert_eq!(
        ev[15].tool.as_ref().unwrap().category,
        ToolCategory::FileRead
    );
    assert_eq!(
        ev[15].paths[0].repo_relative.as_deref(),
        Some("docs/diagram.png")
    );
    assert_eq!(ev[17].tool.as_ref().unwrap().category, ToolCategory::Web);
    assert_eq!(ev[19].tool.as_ref().unwrap().name, "image_generation");
    assert_eq!(attr(&ev[20], "image_count"), 1);
}

#[test]
fn turns_carry_token_usage_as_numbers() {
    let import = parse("modern_turn", CaptureMode::LocalSemantic, true);
    let stop = import
        .events
        .iter()
        .find(|e| e.kind == TurnStopped)
        .expect("turn stopped");
    assert_eq!(stop.duration_ms, Some(24000));
    assert_eq!(attr(stop, "output_tokens"), 450);
    assert_eq!(provider_attr(stop, "input_tokens"), 2100);
    assert_eq!(provider_attr(stop, "cached_input_tokens"), 1500);
    assert_eq!(provider_attr(stop, "reasoning_output_tokens"), 120);
    assert_eq!(provider_attr(stop, "total_tokens"), 2550);
    assert_eq!(provider_attr(stop, "time_to_first_token_ms"), 800);
    assert_eq!(provider_attr(stop, "context_window"), 272000);
    // The turns after it used nothing: no usage attached.
    let failed: Vec<&Event> = import
        .events
        .iter()
        .filter(|e| e.kind == TurnFailed)
        .collect();
    assert!(
        failed
            .iter()
            .all(|e| e.attrs.get("output_tokens").is_none())
    );
}

#[test]
fn interrupts_failures_rollbacks_and_unknown_types() {
    let import = parse("modern_turn", CaptureMode::LocalSemantic, true);
    let aborted = import
        .events
        .iter()
        .find(|e| e.provider_event_name == "transcript:event_msg:turn_aborted")
        .unwrap();
    assert_eq!(aborted.kind, TurnFailed);
    let o = aborted.outcome.as_ref().unwrap();
    assert_eq!(
        (o.status, o.class.as_deref()),
        (OutcomeStatus::Cancelled, Some("interrupted"))
    );
    assert_eq!(attr(aborted, "error_class"), "interrupted");
    assert_eq!(attr(aborted, "reason"), "interrupted");
    assert_eq!(aborted.duration_ms, Some(1200));

    let failed = import
        .events
        .iter()
        .rfind(|e| e.kind == TurnFailed)
        .unwrap();
    assert_eq!(
        failed.outcome.as_ref().unwrap().class.as_deref(),
        Some("usage_limit_exceeded"),
        "the class Codex names"
    );
    assert!(
        failed
            .content
            .as_ref()
            .unwrap()
            .error
            .as_deref()
            .unwrap()
            .starts_with("CANARY_TURN_ERROR")
    );

    let note = import
        .events
        .iter()
        .find(|e| e.kind == Notification)
        .unwrap();
    assert_eq!(attr(note, "notification_type"), "thread_rolled_back");
    assert_eq!(provider_attr(note, "rolled_back_turns"), 1);

    let unknown = import.events.iter().find(|e| e.kind == Unknown).unwrap();
    assert_eq!(
        unknown.provider_event_name,
        "transcript:event_msg:frobnicate"
    );
    assert!(
        unknown.content.is_none(),
        "an unknown event carries no content"
    );
    assert!(
        !serde_json::to_string(unknown).unwrap().contains(CANARY),
        "its payload is not copied"
    );
}

#[test]
fn compaction_is_one_event_with_its_summary_as_content() {
    let import = parse("modern_turn", CaptureMode::LocalSemantic, true);
    let c: Vec<&Event> = import
        .events
        .iter()
        .filter(|e| e.kind == CompactionFinished)
        .collect();
    assert_eq!(c.len(), 1, "compacted, not its two mirrors");
    assert_eq!(attr(c[0], "trigger"), "transcript_compacted");
    assert_eq!(provider_attr(c[0], "window_number"), 2);
    assert_eq!(provider_attr(c[0], "replacement_items"), 3);
    assert!(
        c[0].content.as_ref().unwrap().extra["summary"]
            .as_str()
            .unwrap()
            .starts_with("CANARY_COMPACT_SUMMARY")
    );
}

// ---------------------------------------------------------------------------
// classic_turn (response-item style)
// ---------------------------------------------------------------------------

#[test]
fn classic_turn_kinds_and_order() {
    let import = parse("classic_turn", CaptureMode::LocalSemantic, true);
    assert_eq!(
        kinds(&import),
        vec![
            SessionStarted,
            PromptSubmitted,
            AgentMessage,
            ToolCallStarted, // git status
            ToolCallFinished,
            ToolCallStarted,  // cargo build
            ToolCallStarted,  // npm run dev
            ToolCallFailed,   // cargo build, exit 1
            ToolCallFinished, // npm run dev, still running
            ToolCallStarted,  // write_stdin
            ToolCallFailed,
            ToolCallStarted, // apply_patch
            ToolCallFinished,
            ToolCallStarted, // web search
            ToolCallFinished,
            ToolCallStarted, // view_image
            ToolCallFinished,
            ToolCallStarted, // image generation
            ToolCallFinished,
            ToolCallStarted, // spawn_agent
            ToolCallFinished,
            ToolCallStarted, // MCP (older event)
            ToolCallFinished,
            ToolCallFinished, // an output whose call was never seen
            AgentMessage,
            CompactionFinished,
            TurnStopped,
            SessionEnded,
        ]
    );
    assert!(
        import.summary.warnings.is_empty(),
        "{:?}",
        import.summary.warnings
    );
    assert_eq!(import.summary.stats.unknown_entries, 0);
    assert_eq!(
        import.summary.provider_session_id.as_deref(),
        Some("33333333-3333-4333-8333-333333333333"),
        "a rollout without session_id is its thread id"
    );
}

#[test]
fn response_item_tools_pair_by_call_id() {
    let import = parse("classic_turn", CaptureMode::LocalSemantic, true);
    let ev = &import.events;
    let (start, end) = (&ev[3], &ev[4]);
    for e in [start, end] {
        let t = e.tool.as_ref().unwrap();
        assert_eq!(
            (t.name.as_str(), t.category, t.call_id.as_deref()),
            ("exec_command", ToolCategory::Shell, Some("call_ec01"))
        );
        assert_eq!(attr(e, "command_category"), "git");
        assert_eq!(attr(e, "git_subcommand"), "status");
    }
    assert_eq!(end.outcome.as_ref().unwrap().exit_code, Some(0));
    assert_eq!(end.duration_ms, Some(52), "from `Wall time`");

    // Output order differs from call order: pairing is by call id.
    let build_end = &ev[7];
    assert_eq!(
        build_end.tool.as_ref().unwrap().call_id.as_deref(),
        Some("call_ec02")
    );
    assert_eq!(build_end.kind, ToolCallFailed);
    assert_eq!(build_end.outcome.as_ref().unwrap().exit_code, Some(1));

    // A process that is still running is not a failure.
    let dev_end = &ev[8];
    assert_eq!(dev_end.kind, ToolCallFinished);
    assert_eq!(provider_attr(dev_end, "still_running"), &Value::Bool(true));

    // write_stdin on a closed session failed.
    assert_eq!(ev[10].kind, ToolCallFailed);

    // apply_patch: paths and lines from the patch, exit code from the output.
    let patch = &ev[11];
    assert_eq!(patch.paths.len(), 2);
    assert_eq!(attr(patch, "lines_added"), 3);
    assert_eq!(attr(patch, "lines_removed"), 1);
    assert_eq!(ev[12].outcome.as_ref().unwrap().exit_code, Some(0));
    assert_eq!(ev[12].duration_ms, Some(100));

    // The orphan output still becomes an event, named `unknown`.
    let orphan = &ev[23];
    assert_eq!(orphan.tool.as_ref().unwrap().name, "unknown");
    assert_eq!(
        orphan.tool.as_ref().unwrap().call_id.as_deref(),
        Some("call_orphan")
    );
}

#[test]
fn classic_turn_mirrors_and_duplicates_do_not_double_count() {
    let import = parse("classic_turn", CaptureMode::LocalSemantic, true);
    let s = &import.summary.stats;
    assert_eq!((s.prompts, s.messages), (1, 2));
    let web: Vec<&Event> = import
        .events
        .iter()
        .filter(|e| e.tool.as_ref().is_some_and(|t| t.name == "web_search"))
        .collect();
    assert_eq!(
        web.len(),
        2,
        "web_search_call is a mirror of web_search_end"
    );
    let patches = import
        .events
        .iter()
        .filter(|e| e.tool.as_ref().is_some_and(|t| t.name == "apply_patch"))
        .count();
    assert_eq!(patches, 2, "patch_apply_end is a mirror of the call output");
    let turn = import
        .events
        .iter()
        .find(|e| e.kind == TurnStopped)
        .unwrap();
    assert_eq!(attr(turn, "output_tokens"), 300);
    assert_eq!(provider_attr(turn, "input_tokens"), 1800);
    assert_eq!(provider_attr(turn, "total_tokens"), 2100);
}

// ---------------------------------------------------------------------------
// subagent_thread
// ---------------------------------------------------------------------------

#[test]
fn a_subagent_thread_skips_the_inherited_history_and_joins_its_parent() {
    let import = parse("subagent_thread", CaptureMode::LocalSemantic, true);
    assert_eq!(
        kinds(&import),
        vec![
            SubagentStarted,
            PromptSubmitted,
            ToolCallStarted,
            ToolCallFinished,
            AgentMessage,
            TurnStopped,
            SubagentStopped,
        ]
    );
    let s = &import.summary.stats;
    assert_eq!(s.inherited_lines, 5, "ordinals 1-5 repeat the parent");
    assert!(
        !serde_json::to_string(&import.events)
            .unwrap()
            .contains("INHERITED"),
        "the parent's own rollout holds those lines"
    );
    for ev in &import.events {
        assert_eq!(
            ev.provider_session_id,
            "44444444-4444-4444-8444-444444444444"
        );
        assert_eq!(
            ev.agent.provider_agent_id.as_deref(),
            Some("55555555-5555-4555-8555-555555555555")
        );
        assert_eq!(ev.agent.agent_type.as_deref(), Some("explorer"));
        assert!(ev.agent.parent_agent_id.is_some());
        assert_eq!(attr(ev, "is_subagent"), &Value::Bool(true));
        assert_eq!(attr(ev, "is_sidechain"), &Value::Bool(true));
    }
    assert_eq!(
        provider_attr(&import.events[0], "parent_thread_id"),
        "44444444-4444-4444-8444-444444444444"
    );
    assert!(
        import
            .events
            .iter()
            .all(|e| e.kind != SessionStarted && e.kind != SessionEnded)
    );
}

// ---------------------------------------------------------------------------
// Robustness
// ---------------------------------------------------------------------------

fn envelope(ts: &str, top: &str, payload: Value) -> String {
    serde_json::json!({"timestamp": ts, "type": top, "payload": payload}).to_string()
}

fn meta_line() -> String {
    envelope(
        "2026-08-28T08:00:00.000Z",
        "session_meta",
        serde_json::json!({"id": "s-robust", "cwd": PROJECT_ROOT, "cli_version": "0.1.0", "source": "cli"}),
    )
}

#[test]
fn a_partial_last_line_and_malformed_lines_are_counted_not_fatal() {
    let prompt = envelope(
        "2026-08-28T08:00:01.000Z",
        "event_msg",
        serde_json::json!({"type": "user_message", "message": "CANARY_PROMPT hello?"}),
    );
    let text = format!(
        "{}\nthis is not json\n[1,2,3]\n\n{prompt}\n{{\"timestamp\":\"2026-08-28T08:00:02.000Z\",\"type\":\"event_msg\",\"payload\":{{\"type\":\"agent_mess",
        meta_line()
    );
    let import = parse_text(&text);
    let s = &import.summary.stats;
    assert_eq!(
        (s.malformed_lines, s.partial_tail, s.oversized_lines),
        (2, 1, 0)
    );
    assert_eq!(s.lines_skipped(), 3);
    assert_eq!(
        kinds(&import),
        vec![SessionStarted, PromptSubmitted, SessionEnded]
    );
    assert!(
        import
            .summary
            .warnings
            .iter()
            .any(|w| w.contains("invalid JSON"))
    );
    assert!(
        import
            .summary
            .warnings
            .iter()
            .any(|w| w.contains("middle of a line"))
    );

    // The same line, complete and newline-terminated at EOF, is an event.
    let whole = format!("{}\n{prompt}", meta_line());
    let import = parse_text(&whole);
    assert_eq!(
        kinds(&import),
        vec![SessionStarted, PromptSubmitted, SessionEnded],
        "a final line without a newline that parses is kept"
    );
    assert_eq!(import.summary.stats.partial_tail, 0);
}

#[test]
fn unknown_shapes_become_content_free_events_with_the_type_name() {
    let text = [
        meta_line(),
        envelope("2026-08-28T08:00:01.000Z", "brand_new_record", serde_json::json!({"type": "x", "secret": "CANARY_SECRET"})),
        envelope("2026-08-28T08:00:02.000Z", "event_msg", serde_json::json!({"type": "new_event", "text": "CANARY_SECRET"})),
        envelope("2026-08-28T08:00:03.000Z", "response_item", serde_json::json!({"type": "new_item", "text": "CANARY_SECRET"})),
        envelope("2026-08-28T08:00:04.000Z", "event_msg", serde_json::json!({"type": "item_completed", "item": {"type": "NewItem", "text": "CANARY_SECRET"}})),
        envelope("2026-08-28T08:00:05.000Z", "event_msg", serde_json::json!({"type": "weird type/with spaces"})),
        r#"{"timestamp":"2026-08-28T08:00:06.000Z","payload":{}}"#.to_string(),
    ]
    .join("\n");
    let import = parse_text(&text);
    let names: Vec<&str> = import
        .events
        .iter()
        .filter(|e| e.kind == Unknown)
        .map(|e| e.provider_event_name.as_str())
        .collect();
    assert_eq!(
        names,
        vec![
            "transcript:brand_new_record",
            "transcript:event_msg:new_event",
            "transcript:response_item:new_item",
            "transcript:event_msg:item_completed:NewItem",
            "transcript:event_msg:weird_type_with_spaces",
            "transcript:untyped",
        ]
    );
    assert_eq!(import.summary.stats.unknown_entries, 6);
    let serialised = serde_json::to_string(&import.events).unwrap();
    assert!(
        !serialised.contains(CANARY),
        "unknown payloads are never copied"
    );
}

#[test]
fn unknown_events_are_capped_per_file() {
    let mut lines = vec![meta_line()];
    for i in 0..1200 {
        lines.push(envelope(
            "2026-08-28T08:00:01.000Z",
            "event_msg",
            serde_json::json!({"type": format!("flood_{i}")}),
        ));
    }
    let import = parse_text(&lines.join("\n"));
    assert_eq!(import.summary.stats.unknown_entries, 1200);
    assert_eq!(
        import.events.iter().filter(|e| e.kind == Unknown).count(),
        1000,
        "the rest are counted, not stored"
    );
}

#[test]
fn a_huge_line_is_recognised_and_skipped_without_being_read_whole() {
    let call = envelope(
        "2026-08-28T08:00:01.000Z",
        "response_item",
        serde_json::json!({"type": "function_call", "name": "exec_command", "arguments": "{\"cmd\":\"ls\"}", "call_id": "call_big"}),
    );
    let huge_output = format!(
        "{{\"timestamp\":\"2026-08-28T08:00:02.000Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"function_call_output\",\"call_id\":\"call_big\",\"output\":\"{}\"}}}}",
        "CANARY_BIG".repeat(50_000)
    );
    let huge_compacted = format!(
        "{{\"timestamp\":\"2026-08-28T08:00:03.000Z\",\"type\":\"compacted\",\"payload\":{{\"message\":\"{}\",\"replacement_history\":[]}}}}",
        "x".repeat(600_000)
    );
    let huge_unknown = format!(
        "{{\"timestamp\":\"2026-08-28T08:00:04.000Z\",\"type\":\"mystery\",\"payload\":{{\"blob\":\"{}\"}}}}",
        "y".repeat(600_000)
    );
    let huge_reasoning = format!(
        "{{\"timestamp\":\"2026-08-28T08:00:04.500Z\",\"type\":\"response_item\",\"payload\":{{\"type\":\"reasoning\",\"summary\":[],\"encrypted_content\":\"{}\"}}}}",
        "z".repeat(600_000)
    );
    let after = envelope(
        "2026-08-28T08:00:05.000Z",
        "event_msg",
        serde_json::json!({"type": "user_message", "message": "CANARY_AFTER"}),
    );
    let text = [
        meta_line(),
        call,
        huge_output,
        huge_compacted,
        huge_unknown,
        huge_reasoning,
        after,
    ]
    .join("\n");
    let opts = CodexRolloutOptions {
        max_line_bytes: 64 * 1024,
        ..options_for("huge", true)
    };
    let import = parse_codex_rollout_to_vec(
        Cursor::new(text.into_bytes()),
        &context(CaptureMode::LocalSemantic),
        &opts,
    );
    let s = &import.summary.stats;
    assert_eq!(
        (s.oversized_lines, s.malformed_lines),
        (3, 0),
        "an oversized reasoning line is recognised bookkeeping, not a loss"
    );
    assert_eq!(s.compactions, 1);
    assert_eq!(
        kinds(&import),
        vec![
            SessionStarted,
            ToolCallStarted,
            ToolCallFinished,
            CompactionFinished,
            PromptSubmitted,
            SessionEnded,
        ]
    );
    let finish = &import.events[2];
    assert_eq!(
        finish.tool.as_ref().unwrap().call_id.as_deref(),
        Some("call_big")
    );
    assert_eq!(attr(finish, "tool_output_truncated"), &Value::Bool(true));
    assert_eq!(
        finish.outcome.as_ref().unwrap().status,
        OutcomeStatus::Unknown
    );
    assert!(
        !serde_json::to_string(&import.events)
            .unwrap()
            .contains("CANARY_BIG"),
        "the oversized body is not kept"
    );
    assert!(
        import
            .summary
            .warnings
            .iter()
            .any(|w| w.contains("over the 65536 byte limit"))
    );
    // The line after the huge ones is intact.
    assert_eq!(
        import.events[4].content.as_ref().unwrap().prompt.as_deref(),
        Some("CANARY_AFTER")
    );
}

#[test]
fn a_file_without_session_meta_uses_the_file_name_hint() {
    let text = envelope(
        "2026-08-28T08:00:01.000Z",
        "event_msg",
        serde_json::json!({"type": "user_message", "message": "hello"}),
    );
    let import = parse_text(&text);
    assert_eq!(
        import.summary.provider_session_id.as_deref(),
        Some("inline-stem")
    );
    assert_eq!(
        kinds(&import),
        vec![SessionStarted, PromptSubmitted, SessionEnded]
    );
    assert!(
        import
            .summary
            .warnings
            .iter()
            .any(|w| w.contains("no session_meta first"))
    );
    let none = parse_codex_rollout_to_vec(
        Cursor::new(text.into_bytes()),
        &context(CaptureMode::LocalSemantic),
        &CodexRolloutOptions::default(),
    );
    assert_eq!(none.summary.provider_session_id, None);
    assert!(
        none.events
            .iter()
            .all(|e| e.provider_session_id == "unknown")
    );
}

#[test]
fn rollouts_that_wrote_response_items_bare_still_parse() {
    let text = [
        meta_line(),
        serde_json::json!({"type": "function_call", "name": "shell", "arguments": "{\"command\":[\"bash\",\"-lc\",\"cargo test\"]}", "call_id": "c1"}).to_string(),
        serde_json::json!({"type": "function_call_output", "call_id": "c1", "output": "{\"output\":\"ok\",\"metadata\":{\"exit_code\":0}}"}).to_string(),
    ]
    .join("\n");
    let import = parse_text(&text);
    assert_eq!(
        kinds(&import),
        vec![
            SessionStarted,
            ToolCallStarted,
            ToolCallFinished,
            SessionEnded
        ]
    );
    assert_eq!(attr(&import.events[1], "command_category"), "test");
    assert_eq!(
        import.events[2].outcome.as_ref().unwrap().exit_code,
        Some(0)
    );
}

#[test]
fn the_end_of_a_live_session_is_optional() {
    let opts = CodexRolloutOptions {
        emit_session_end: false,
        ..options_for("live", true)
    };
    let import = parse_codex_rollout_to_vec(
        Cursor::new(bytes("modern_turn")),
        &context(CaptureMode::LocalSemantic),
        &opts,
    );
    assert!(import.events.iter().all(|e| e.kind != SessionEnded));
    let sub = parse_codex_rollout_to_vec(
        Cursor::new(bytes("subagent_thread")),
        &context(CaptureMode::LocalSemantic),
        &opts,
    );
    assert!(sub.events.iter().all(|e| e.kind != SubagentStopped));
}

#[test]
fn a_failing_sink_stops_the_parse() {
    let mut seen = 0usize;
    let result = super::codex::parse_codex_rollout(
        Cursor::new(bytes("modern_turn")),
        &context(CaptureMode::LocalSemantic),
        &options_for("modern_turn", true),
        |_| {
            seen += 1;
            if seen == 3 { Err("disk full") } else { Ok(()) }
        },
    );
    assert_eq!(result.unwrap_err(), "disk full");
    assert_eq!(seen, 3);
}

#[test]
fn nothing_panics_on_garbage() {
    let garbage = [
        "",
        "null",
        "42",
        "{}",
        r#"{"type":"event_msg"}"#,
        r#"{"type":"event_msg","payload":null}"#,
        r#"{"type":"event_msg","payload":{"type":"item_completed"}}"#,
        r#"{"type":"event_msg","payload":{"type":"item_completed","item":7}}"#,
        r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"CommandExecution"}}}"#,
        r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"FileChange","changes":7}}}"#,
        r#"{"type":"event_msg","payload":{"type":"item_completed","item":{"type":"McpToolCall","result":"x","error":7}}}"#,
        r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":"x"}}}"#,
        r#"{"type":"event_msg","payload":{"type":"task_complete","duration_ms":"slow","error":[]}}"#,
        r#"{"type":"event_msg","payload":{"type":"user_message","message":7,"images":"x"}}"#,
        r#"{"type":"response_item","payload":{"type":"function_call","arguments":7,"call_id":7}}"#,
        r#"{"type":"response_item","payload":{"type":"function_call_output","output":{"a":1}}}"#,
        r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","output":[1,"x",{"type":7}]}}"#,
        r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":"not-a-list"}}"#,
        r#"{"type":"compacted","payload":{"message":7,"replacement_history":"x"}}"#,
        r#"{"type":"turn_context","payload":{"cwd":7,"model":[],"turn_id":{}}}"#,
        r#"{"type":"session_meta","payload":{"source":{"subagent":7},"git":7,"id":7}}"#,
        r#"{"type":"session_meta","payload":{"parent_thread_id":"p","subagent_history_start_ordinal":"x"}}"#,
        "{\"timestamp\":\"2026-08-28T08:00:00.000Z\",\"ordinal\":18446744073709551615,\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"x\"}}",
    ];
    for mode in [CaptureMode::LocalSemantic, CaptureMode::MetadataOnly] {
        let import = parse_codex_rollout_to_vec(
            Cursor::new(garbage.join("\n").into_bytes()),
            &context(mode),
            &CodexRolloutOptions::for_capture_mode(mode),
        );
        if mode == CaptureMode::MetadataOnly {
            assert!(import.events.iter().all(|e| e.content.is_none()));
        }
    }
    // Binary junk, invalid UTF-8 and NULs.
    let mut bin = meta_line().into_bytes();
    bin.extend_from_slice(b"\n\xff\xfe\x00\x01 not json\n");
    bin.extend_from_slice(b"{\"timestamp\":\"2026-08-28T08:00:01.000Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"caf\xe9\"}}\n");
    let import = parse_codex_rollout_to_vec(
        Cursor::new(bin),
        &context(CaptureMode::LocalSemantic),
        &CodexRolloutOptions::default(),
    );
    assert_eq!(import.summary.stats.malformed_lines, 1);
    assert!(
        import.events.iter().any(|e| e.kind == PromptSubmitted),
        "lossy UTF-8 keeps the line"
    );
}

#[test]
fn meta_peek_gives_the_project_facts_without_parsing_the_file() {
    let first = bytes("modern_turn");
    let line = first.split(|b| *b == b'\n').next().unwrap();
    let meta = peek_rollout_meta(line).unwrap();
    assert_eq!(
        meta.session_id.as_deref(),
        Some("22222222-2222-4222-8222-222222222222")
    );
    assert_eq!(meta.cwd.as_deref(), Some(PROJECT_ROOT));
    assert_eq!(meta.git_remote.as_deref(), Some(PROJECT_REMOTE));
    assert_eq!(meta.git_branch.as_deref(), Some("main"));
    assert!(!meta.is_subagent());
    assert!(peek_rollout_meta(b"{\"type\":\"turn_context\"}").is_none());
    assert!(peek_rollout_meta(b"garbage").is_none());
    let sub = bytes("subagent_thread");
    let line = sub.split(|b| *b == b'\n').next().unwrap();
    let meta = peek_rollout_meta(line).unwrap();
    assert!(meta.is_subagent());
    assert_eq!(
        meta.session_id.as_deref(),
        Some("44444444-4444-4444-8444-444444444444")
    );
    assert_eq!(meta.history_start_ordinal, Some(6));
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
            for private in ["must-not-appear", ".jsonl", "Zeno"] {
                assert!(!serialised.contains(private), "{name}: `{private}` leaked");
            }
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
            // Paths are metadata (they stay in metadata-only mode); the
            // canaries never sit in a path.
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

#[test]
fn content_is_bounded() {
    let big = "z".repeat(400 * 1024);
    let text = [
        meta_line(),
        envelope(
            "2026-08-28T08:00:01.000Z",
            "event_msg",
            serde_json::json!({"type": "user_message", "message": big}),
        ),
        envelope(
            "2026-08-28T08:00:02.000Z",
            "response_item",
            serde_json::json!({"type": "function_call", "name": "exec_command", "arguments": serde_json::json!({"cmd": big}).to_string(), "call_id": "c1"}),
        ),
        envelope(
            "2026-08-28T08:00:03.000Z",
            "response_item",
            serde_json::json!({"type": "function_call_output", "call_id": "c1", "output": format!("Process exited with code 0\n{big}")}),
        ),
    ]
    .join("\n");
    let import = parse_text(&text);
    let prompt = &import.events[1];
    assert!(
        prompt
            .content
            .as_ref()
            .unwrap()
            .prompt
            .as_ref()
            .unwrap()
            .len()
            <= 256 * 1024
    );
    assert_eq!(
        attr(prompt, "prompt_chars"),
        400 * 1024,
        "the count is of the whole prompt"
    );
    let end = &import.events[3];
    let out = end
        .content
        .as_ref()
        .unwrap()
        .tool_output
        .as_ref()
        .unwrap()
        .as_str()
        .unwrap();
    assert!(out.len() <= 64 * 1024);
    assert_eq!(attr(end, "tool_output_truncated"), &Value::Bool(true));
    let started = &import.events[2];
    assert!(serde_json::to_string(started).unwrap().len() < 200 * 1024);
    assert_eq!(
        provider_attr(started, "input_truncated"),
        &Value::Bool(true)
    );
}
