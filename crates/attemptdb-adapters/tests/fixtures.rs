//! Fixture-driven contract tests for every provider adapter.
//!
//! Fixtures live in `fixtures/providers/<provider>/<name>.json`. Each one must
//! have an entry in [`EXPECTATIONS`] (kind, tool, outcome, paths) and a golden
//! normalised envelope next to it (`<name>.golden.json`). Golden files are
//! generated when missing and regenerated with `UPDATE_GOLDEN=1`.

use attemptdb_adapters::privacy;
use attemptdb_adapters::{ADAPTER_VERSION, Adapter, AdapterError, CaptureContext, adapter_for};
use attemptdb_core::event::Provider;
use attemptdb_core::{
    AgentId, CaptureMode, DeviceId, Event, EventId, EventKind, OutcomeStatus, ProjectRef,
    Timestamp, ToolCategory,
};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

const PROJECT_ROOT: &str = "/home/dev/example/project";
const PROJECT_REMOTE: &str = "git@github.com:example/project.git";
const CAPTURED_AT: Timestamp = Timestamp::from_micros(1_787_904_000_000_000); // 2026-08-28T08:00:00Z
const PROVIDERS: &[&str] = &["claude_code", "codex", "cursor", "gemini_cli"];

/// Strings that must never appear in a metadata-only envelope or in attrs.
const GLOBAL_CANARIES: &[&str] = &[
    "CANARY_",
    "sk-live-",
    "SECRET_TOKEN",
    ".jsonl",
    "@example.com",
    "noreply@anthropic.com",
    "transcript_path",
    "/home/dev/.claude",
    "/home/dev/.gemini",
];

/// Attribute keys allowed to carry a normalised absolute path.
const PATH_ATTRS: &[&str] = &["cwd", "previous_cwd", "worktree_path"];

struct Expect {
    provider: &'static str,
    name: &'static str,
    kind: EventKind,
    tool: Option<(&'static str, ToolCategory)>,
    outcome: Option<(OutcomeStatus, Option<&'static str>)>,
    paths: &'static [&'static str],
}

const fn ex(
    provider: &'static str,
    name: &'static str,
    kind: EventKind,
    tool: Option<(&'static str, ToolCategory)>,
    outcome: Option<(OutcomeStatus, Option<&'static str>)>,
    paths: &'static [&'static str],
) -> Expect {
    Expect {
        provider,
        name,
        kind,
        tool,
        outcome,
        paths,
    }
}

const OK: Option<(OutcomeStatus, Option<&str>)> = Some((OutcomeStatus::Success, None));
const DENIED: Option<(OutcomeStatus, Option<&str>)> = Some((OutcomeStatus::Denied, None));
const fn failed(class: &'static str) -> Option<(OutcomeStatus, Option<&'static str>)> {
    Some((OutcomeStatus::Failure, Some(class)))
}

use EventKind::*;
use ToolCategory::*;

const EXPECTATIONS: &[Expect] = &[
    // --- Claude Code --------------------------------------------------------
    ex(
        "claude_code",
        "session_start",
        SessionStarted,
        None,
        None,
        &[],
    ),
    ex("claude_code", "session_end", SessionEnded, None, None, &[]),
    ex(
        "claude_code",
        "user_prompt_submit_ko",
        PromptSubmitted,
        None,
        None,
        &[],
    ),
    ex(
        "claude_code",
        "user_prompt_submit_en_long",
        PromptSubmitted,
        None,
        None,
        &[],
    ),
    ex(
        "claude_code",
        "post_tool_use_edit",
        ToolCallFinished,
        Some(("Edit", FileEdit)),
        OK,
        &["src/components/Button.tsx"],
    ),
    ex(
        "claude_code",
        "post_tool_use_edit_duration_ms",
        ToolCallFinished,
        Some(("Edit", FileEdit)),
        OK,
        &["src/Card.tsx"],
    ),
    ex(
        "claude_code",
        "post_tool_use_edit_duration_alt",
        ToolCallFinished,
        Some(("Edit", FileEdit)),
        OK,
        &["src/Card.tsx"],
    ),
    ex(
        "claude_code",
        "post_tool_use_write",
        ToolCallFinished,
        Some(("Write", FileWrite)),
        OK,
        &["src/utils/helper.ts"],
    ),
    ex(
        "claude_code",
        "post_tool_use_bash_git_commit",
        ToolCallFinished,
        Some(("Bash", Shell)),
        OK,
        &[],
    ),
    ex(
        "claude_code",
        "post_tool_use_bash_git_commit_heredoc",
        ToolCallFinished,
        Some(("Bash", Shell)),
        OK,
        &[],
    ),
    ex(
        "claude_code",
        "post_tool_use_bash_npm_test",
        ToolCallFinished,
        Some(("Bash", Shell)),
        OK,
        &[],
    ),
    ex(
        "claude_code",
        "post_tool_use_bash_unknown",
        ToolCallFinished,
        Some(("Bash", Shell)),
        OK,
        &[],
    ),
    ex(
        "claude_code",
        "post_tool_use_mcp",
        ToolCallFinished,
        Some(("mcp__github__create_issue", Mcp)),
        OK,
        &[],
    ),
    ex(
        "claude_code",
        "post_tool_use_task_subagent",
        ToolCallFinished,
        Some(("Task", Subagent)),
        OK,
        &[],
    ),
    ex(
        "claude_code",
        "post_tool_use_failure_string_mismatch",
        ToolCallFailed,
        Some(("Edit", FileEdit)),
        failed("string_mismatch"),
        &["src/old.ts"],
    ),
    ex(
        "claude_code",
        "post_tool_use_failure_nonzero_exit",
        ToolCallFailed,
        Some(("Bash", Shell)),
        failed("nonzero_exit"),
        &[],
    ),
    ex(
        "claude_code",
        "pre_tool_use_bash",
        ToolCallStarted,
        Some(("Bash", Shell)),
        None,
        &[],
    ),
    ex(
        "claude_code",
        "permission_request_bash",
        PermissionRequested,
        Some(("Bash", Shell)),
        None,
        &[],
    ),
    ex(
        "claude_code",
        "permission_denied_edit",
        PermissionDenied,
        Some(("Edit", FileEdit)),
        DENIED,
        &["src/config.ts"],
    ),
    ex(
        "claude_code",
        "notification_permission_prompt",
        Notification,
        None,
        None,
        &[],
    ),
    ex(
        "claude_code",
        "stop_last_assistant_message",
        TurnStopped,
        None,
        None,
        &[],
    ),
    ex(
        "claude_code",
        "stop_failure_rate_limit",
        TurnFailed,
        None,
        failed("rate_limit"),
        &[],
    ),
    ex(
        "claude_code",
        "subagent_start",
        SubagentStarted,
        None,
        None,
        &[],
    ),
    ex(
        "claude_code",
        "subagent_stop",
        SubagentStopped,
        None,
        None,
        &[],
    ),
    ex("claude_code", "task_created", TaskCreated, None, None, &[]),
    ex(
        "claude_code",
        "task_completed",
        TaskCompleted,
        None,
        None,
        &[],
    ),
    ex(
        "claude_code",
        "pre_compact_manual",
        CompactionStarted,
        None,
        None,
        &[],
    ),
    ex(
        "claude_code",
        "config_change",
        ConfigChanged,
        None,
        None,
        &[".claude/settings.json"],
    ),
    ex(
        "claude_code",
        "cwd_changed",
        CwdChanged,
        None,
        None,
        &["/home/dev/example/project"],
    ),
    ex(
        "claude_code",
        "file_changed",
        FileChanged,
        None,
        None,
        &["src/lib.rs"],
    ),
    ex(
        "claude_code",
        "worktree_create",
        WorktreeCreated,
        None,
        None,
        &[".claude/worktrees/feature-x"],
    ),
    ex(
        "claude_code",
        "unknown_event_message_display",
        Unknown,
        None,
        None,
        &[],
    ),
    ex(
        "claude_code",
        "canary_bash_secret",
        ToolCallFinished,
        Some(("Bash", Shell)),
        OK,
        &[],
    ),
    ex(
        "claude_code",
        "canary_prompt_secret",
        PromptSubmitted,
        None,
        None,
        &[],
    ),
    ex(
        "claude_code",
        "canary_write_secret",
        ToolCallFinished,
        Some(("Write", FileWrite)),
        OK,
        &["secret.ts"],
    ),
    // --- Codex --------------------------------------------------------------
    ex("codex", "session_start", SessionStarted, None, None, &[]),
    ex(
        "codex",
        "user_prompt_submit",
        PromptSubmitted,
        None,
        None,
        &[],
    ),
    ex(
        "codex",
        "pre_tool_use_shell_array",
        ToolCallStarted,
        Some(("shell", Shell)),
        None,
        &[],
    ),
    ex(
        "codex",
        "post_tool_use_bash_commit",
        ToolCallFinished,
        Some(("Bash", Shell)),
        OK,
        &[],
    ),
    ex(
        "codex",
        "post_tool_use_apply_patch",
        ToolCallFinished,
        Some(("apply_patch", FileEdit)),
        OK,
        &["src/sync/client.rs", "src/sync/retry.rs"],
    ),
    ex(
        "codex",
        "permission_request_shell",
        PermissionRequested,
        Some(("shell", Shell)),
        None,
        &[],
    ),
    ex("codex", "subagent_start", SubagentStarted, None, None, &[]),
    ex("codex", "stop", TurnStopped, None, None, &[]),
    ex(
        "codex",
        "canary_shell_secret",
        ToolCallFinished,
        Some(("shell", Shell)),
        OK,
        &[],
    ),
    // --- Cursor -------------------------------------------------------------
    ex("cursor", "session_start", SessionStarted, None, None, &[]),
    ex("cursor", "session_end", SessionEnded, None, None, &[]),
    ex(
        "cursor",
        "before_submit_prompt",
        PromptSubmitted,
        None,
        None,
        &["components/slime/SlimeCanvas.tsx"],
    ),
    ex("cursor", "stop", TurnStopped, None, None, &[]),
    ex(
        "cursor",
        "after_file_edit",
        ToolCallFinished,
        Some(("Edit", FileEdit)),
        OK,
        &["src/components/Button.tsx"],
    ),
    ex(
        "cursor",
        "after_shell_execution_git_commit",
        ToolCallFinished,
        Some(("Shell", Shell)),
        OK,
        &[],
    ),
    ex(
        "cursor",
        "after_shell_execution_exit_code",
        ToolCallFinished,
        Some(("Shell", Shell)),
        OK,
        &[],
    ),
    ex(
        "cursor",
        "after_shell_execution_nonzero_exit",
        ToolCallFailed,
        Some(("Shell", Shell)),
        failed("nonzero_exit"),
        &[],
    ),
    ex(
        "cursor",
        "post_tool_use_failure_shell",
        ToolCallFailed,
        Some(("Shell", Shell)),
        failed("timeout"),
        &[],
    ),
    ex(
        "cursor",
        "canary_edits_secret",
        ToolCallFinished,
        Some(("Edit", FileEdit)),
        OK,
        &["src/secrets.ts"],
    ),
    ex(
        "cursor",
        "canary_shell_output",
        ToolCallFinished,
        Some(("Shell", Shell)),
        OK,
        &[],
    ),
    // --- Gemini CLI ---------------------------------------------------------
    ex(
        "gemini_cli",
        "session_start",
        SessionStarted,
        None,
        None,
        &[],
    ),
    ex(
        "gemini_cli",
        "before_agent",
        PromptSubmitted,
        None,
        None,
        &[],
    ),
    ex("gemini_cli", "after_agent", TurnStopped, None, None, &[]),
    ex(
        "gemini_cli",
        "before_tool_replace",
        ToolCallStarted,
        Some(("replace", FileEdit)),
        None,
        &["lib/api/todos.ts"],
    ),
    ex(
        "gemini_cli",
        "after_tool_write_file",
        ToolCallFinished,
        Some(("write_file", FileWrite)),
        OK,
        &["lib/api/todos.ts"],
    ),
    ex(
        "gemini_cli",
        "after_tool_run_shell_command",
        ToolCallFinished,
        Some(("run_shell_command", Shell)),
        OK,
        &[],
    ),
    ex(
        "gemini_cli",
        "after_tool_error",
        ToolCallFailed,
        Some(("read_file", FileRead)),
        failed("file_not_found"),
        &["src/missing.ts"],
    ),
    ex(
        "gemini_cli",
        "canary_prompt_secret",
        PromptSubmitted,
        None,
        None,
        &[],
    ),
];

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct Fixture {
    provider: &'static str,
    name: String,
    path: PathBuf,
    payload: Value,
}

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/providers")
}

fn device_id() -> DeviceId {
    DeviceId::derive(&["adapter-tests"])
}

fn context(mode: CaptureMode) -> CaptureContext {
    let device_id = device_id();
    CaptureContext {
        device_id,
        capture_mode: mode,
        project: ProjectRef::derive(PROJECT_ROOT, Some(PROJECT_REMOTE), &device_id),
        captured_at: CAPTURED_AT,
        provider_version: None,
        hook_version: Some("test".into()),
    }
}

fn adapter(provider: &str) -> Box<dyn Adapter> {
    let provider: Provider = provider.parse().expect("infallible");
    adapter_for(&provider).expect("built-in adapter")
}

fn load_fixtures() -> Vec<Fixture> {
    let mut out = Vec::new();
    for provider in PROVIDERS {
        let dir = fixtures_root().join(provider);
        let mut entries: Vec<PathBuf> = fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("read {}: {e}", dir.display()))
            .map(|e| e.expect("dir entry").path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .filter(|p| !p.to_string_lossy().ends_with(".golden.json"))
            .collect();
        entries.sort();
        for path in entries {
            let text = fs::read_to_string(&path).expect("read fixture");
            let payload: Value = serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("{}: invalid JSON: {e}", path.display()));
            let name = path.file_stem().unwrap().to_string_lossy().into_owned();
            out.push(Fixture {
                provider,
                name,
                path,
                payload,
            });
        }
    }
    assert!(
        !out.is_empty(),
        "no fixtures found under {}",
        fixtures_root().display()
    );
    out
}

fn normalise(fixture: &Fixture, mode: CaptureMode) -> Event {
    adapter(fixture.provider)
        .normalise(&context(mode), None, &fixture.payload)
        .unwrap_or_else(|e| panic!("{}: {e}", fixture.path.display()))
}

fn expectation(fixture: &Fixture) -> &'static Expect {
    EXPECTATIONS
        .iter()
        .find(|e| e.provider == fixture.provider && e.name == fixture.name)
        .unwrap_or_else(|| {
            panic!(
                "no expectation for fixture {}/{}",
                fixture.provider, fixture.name
            )
        })
}

fn zeroed_for_golden(mut event: Event) -> Event {
    event.event_id = EventId::nil();
    event.device_id = DeviceId::nil();
    event.captured_at = Timestamp::from_micros(0);
    event
}

// ---------------------------------------------------------------------------
// 1. Every fixture normalises to the expected shape
// ---------------------------------------------------------------------------

#[test]
fn every_fixture_matches_its_expectation() {
    let fixtures = load_fixtures();
    for fixture in &fixtures {
        let expect = expectation(fixture);
        let event = normalise(fixture, CaptureMode::LocalSemantic);
        let label = format!("{}/{}", fixture.provider, fixture.name);

        assert_eq!(event.kind, expect.kind, "{label}: kind");
        assert_eq!(
            event.provider.as_str(),
            fixture.provider,
            "{label}: provider"
        );
        assert_eq!(
            event.adapter_version, ADAPTER_VERSION,
            "{label}: adapter version"
        );
        assert_eq!(
            event.hook_version.as_deref(),
            Some("test"),
            "{label}: hook version"
        );
        assert_eq!(event.captured_at, CAPTURED_AT, "{label}: captured_at");
        assert!(!event.provider_session_id.is_empty(), "{label}: session id");
        assert_eq!(
            event.attrs.get("hook_event_name").and_then(Value::as_str),
            Some(event.provider_event_name.as_str()),
            "{label}: hook_event_name attr"
        );

        let tool = event.tool.as_ref().map(|t| (t.name.as_str(), t.category));
        assert_eq!(tool, expect.tool, "{label}: tool");

        let outcome = event
            .outcome
            .as_ref()
            .map(|o| (o.status, o.class.as_deref()));
        assert_eq!(outcome, expect.outcome, "{label}: outcome");

        let paths: Vec<&str> = event.paths.iter().map(|p| p.display()).collect();
        assert_eq!(paths, expect.paths, "{label}: paths");
    }
    for expect in EXPECTATIONS {
        assert!(
            fixtures
                .iter()
                .any(|f| f.provider == expect.provider && f.name == expect.name),
            "expectation without fixture: {}/{}",
            expect.provider,
            expect.name
        );
    }
}

#[test]
fn derived_metadata_spot_checks() {
    let fixtures = load_fixtures();
    let find = |provider: &str, name: &str| {
        let f = fixtures
            .iter()
            .find(|f| f.provider == provider && f.name == name)
            .unwrap_or_else(|| panic!("missing fixture {provider}/{name}"));
        normalise(f, CaptureMode::LocalSemantic)
    };
    let attr = |e: &Event, k: &str| e.attrs.get(k).cloned().unwrap_or(Value::Null);

    let edit = find("claude_code", "post_tool_use_edit");
    assert_eq!(attr(&edit, "lines_added"), 4);
    assert_eq!(attr(&edit, "lines_removed"), 1);
    assert_eq!(attr(&edit, "file_ext"), "tsx");
    assert_eq!(attr(&edit, "file_is_test"), false);

    let heredoc = find("claude_code", "post_tool_use_bash_git_commit_heredoc");
    assert_eq!(attr(&heredoc, "command_category"), "git");
    assert_eq!(attr(&heredoc, "git_subcommand"), "commit");

    let npm = find("claude_code", "post_tool_use_bash_npm_test");
    assert_eq!(attr(&npm, "command_category"), "test");
    assert!(npm.attrs.get("git_subcommand").is_none());

    let prompt = find("claude_code", "user_prompt_submit_en_long");
    assert_eq!(attr(&prompt, "prompt_has_code_fence"), true);
    assert_eq!(attr(&prompt, "prompt_has_question"), true);
    assert_eq!(attr(&prompt, "prompt_lines"), 9);

    let dur = find("claude_code", "post_tool_use_edit_duration_ms");
    assert_eq!(dur.duration_ms, Some(1250));
    assert_eq!(
        find("claude_code", "post_tool_use_edit_duration_alt").duration_ms,
        Some(800)
    );

    let nonzero = find("claude_code", "post_tool_use_failure_nonzero_exit");
    assert_eq!(nonzero.outcome.as_ref().and_then(|o| o.exit_code), Some(1));
    assert_eq!(attr(&nonzero, "error_class"), "nonzero_exit");
    assert_eq!(
        nonzero
            .content
            .as_ref()
            .and_then(|c| c.error.as_deref())
            .map(|e| e.starts_with("Command failed")),
        Some(true)
    );

    let session = find("claude_code", "session_start");
    assert_eq!(attr(&session, "source"), "startup");
    assert_eq!(session.agent.model.as_deref(), Some("claude-sonnet-4-6"));

    let cwd = find("claude_code", "cwd_changed");
    assert_eq!(attr(&cwd, "cwd"), "~/example/project/crates/core");
    assert_eq!(attr(&cwd, "previous_cwd"), "~/example/project");

    let sub = find("claude_code", "subagent_start");
    let session_key = sub.session_id.to_string();
    assert_eq!(sub.agent.provider_agent_id.as_deref(), Some("agent-7f3a"));
    assert_eq!(sub.agent.agent_type.as_deref(), Some("Explore"));
    assert_eq!(
        sub.agent.agent_id,
        AgentId::derive(&["subagent", &session_key, "agent-7f3a"])
    );
    assert_eq!(
        sub.agent.parent_agent_id,
        Some(AgentId::derive(&["session", &session_key]))
    );
    assert_eq!(attr(&sub, "is_subagent"), true);
    assert_eq!(attr(&sub, "transcript_present"), true);
    assert_eq!(attr(&sub, "permission_mode"), "default");

    let task = find("claude_code", "post_tool_use_task_subagent");
    assert_eq!(attr(&task, "agent_type"), "Explore");

    let stop = find("claude_code", "stop_last_assistant_message");
    assert_eq!(stop.provider_turn_id.as_deref(), Some("prompt-7"));
    assert_eq!(attr(&stop, "stop_hook_active"), false);
    assert!(
        stop.content
            .as_ref()
            .and_then(|c| c.message.as_deref())
            .is_some()
    );

    let codex = find("codex", "pre_tool_use_shell_array");
    assert_eq!(codex.provider_turn_id.as_deref(), Some("turn-1"));
    assert_eq!(attr(&codex, "command_category"), "test");
    assert_eq!(
        codex.content.as_ref().and_then(|c| c.command.as_deref()),
        Some("bash -lc cargo test -p attemptdb-adapters")
    );

    let codex_ok = find("codex", "canary_shell_secret");
    assert_eq!(codex_ok.outcome.as_ref().and_then(|o| o.exit_code), Some(0));

    let cursor_edit = find("cursor", "after_file_edit");
    assert_eq!(attr(&cursor_edit, "lines_added"), 4);
    assert_eq!(attr(&cursor_edit, "lines_removed"), 1);
    assert_eq!(attr(&cursor_edit, "provider")["edit_count"], 1);
    assert_eq!(cursor_edit.provider_version.as_deref(), Some("1.7.11"));
    assert_eq!(cursor_edit.provider_session_id, "conv-cursor-1");

    let cursor_shell = find("cursor", "after_shell_execution_nonzero_exit");
    assert_eq!(cursor_shell.duration_ms, Some(3900));
    assert_eq!(
        cursor_shell.outcome.as_ref().and_then(|o| o.exit_code),
        Some(1)
    );

    let cursor_fail = find("cursor", "post_tool_use_failure_shell");
    assert_eq!(attr(&cursor_fail, "provider")["failure_type"], "timeout");
    assert_eq!(
        cursor_fail.tool.as_ref().and_then(|t| t.call_id.as_deref()),
        Some("tu-1")
    );

    let cursor_prompt = find("cursor", "before_submit_prompt");
    assert_eq!(attr(&cursor_prompt, "provider")["attachment_count"], 1);
    assert_eq!(attr(&cursor_prompt, "prompt_has_question"), false);

    let gemini = find("gemini_cli", "before_agent");
    assert_eq!(
        gemini.observed_at,
        Timestamp::parse("2026-08-28T09:15:00.000Z").unwrap()
    );
    assert_eq!(attr(&gemini, "transcript_present"), true);
    assert_eq!(
        find("gemini_cli", "after_tool_write_file").observed_at,
        CAPTURED_AT
    );

    let gemini_shell = find("gemini_cli", "after_tool_run_shell_command");
    assert_eq!(attr(&gemini_shell, "command_category"), "build");
}

// ---------------------------------------------------------------------------
// 2. Golden envelopes
// ---------------------------------------------------------------------------

#[test]
fn golden_envelopes_match() {
    let update = std::env::var("UPDATE_GOLDEN").is_ok_and(|v| v == "1");
    let mut mismatches = Vec::new();
    for fixture in load_fixtures() {
        let event = zeroed_for_golden(normalise(&fixture, CaptureMode::LocalSemantic));
        let actual = serde_json::to_value(&event).expect("serialise event");
        let golden_path = fixture.path.with_extension("golden.json");
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
        // A golden must round-trip through the canonical model unchanged.
        let back: Event = serde_json::from_value(golden.clone()).expect("golden deserialises");
        assert!(
            back.unknown.is_empty(),
            "{}: unknown fields",
            golden_path.display()
        );
    }
    assert!(
        mismatches.is_empty(),
        "{} golden mismatch(es); run with UPDATE_GOLDEN=1 to regenerate:\n\n{}",
        mismatches.len(),
        mismatches.join("\n\n")
    );
}

// ---------------------------------------------------------------------------
// 3. Privacy canaries
// ---------------------------------------------------------------------------

fn canaries_for(fixture: &Fixture) -> Vec<String> {
    let mut needles: Vec<String> = GLOBAL_CANARIES.iter().map(|s| s.to_string()).collect();
    needles.extend(privacy::collect_content_strings(&fixture.payload));
    needles
}

#[test]
fn metadata_only_events_carry_no_content() {
    for fixture in load_fixtures() {
        let label = format!("{}/{}", fixture.provider, fixture.name);
        let event = normalise(&fixture, CaptureMode::MetadataOnly);
        assert!(event.content.is_none(), "{label}: content must be None");
        assert!(event.raw.is_none(), "{label}: raw must be None");
        assert!(!privacy::has_content_or_raw(&event), "{label}");
        assert_eq!(event.capture_mode, CaptureMode::MetadataOnly);

        let serialised = serde_json::to_string(&event).unwrap();
        if let Some(leak) = privacy::find_leak(&serialised, &canaries_for(&fixture)) {
            panic!("{label}: metadata-only envelope leaks {leak:?}:\n{serialised}");
        }
        assert!(
            !serialised.contains('@'),
            "{label}: metadata-only envelope contains '@'"
        );
    }
}

#[test]
fn attrs_never_carry_content_in_any_mode() {
    for fixture in load_fixtures() {
        let label = format!("{}/{}", fixture.provider, fixture.name);
        for mode in [
            CaptureMode::MetadataOnly,
            CaptureMode::LocalSemantic,
            CaptureMode::FullSync,
        ] {
            let event = normalise(&fixture, mode);
            let attrs = serde_json::to_string(&event.attrs).unwrap();
            if let Some(leak) = privacy::find_leak(&attrs, &canaries_for(&fixture)) {
                panic!("{label} ({mode}): attrs leak {leak:?}:\n{attrs}");
            }
            let hits = privacy::attrs_containing(&event.attrs, "/home/dev", PATH_ATTRS);
            assert!(
                hits.is_empty(),
                "{label} ({mode}): home path in attrs {hits:?}"
            );
            for key in event.attrs.keys() {
                assert!(
                    attemptdb_adapters::common::ALLOWED_ATTR_KEYS.contains(&key.as_str()),
                    "{label}: attr `{key}` is not allowlisted"
                );
            }
        }
    }
}

#[test]
fn retained_raw_never_carries_transcript_paths() {
    for fixture in load_fixtures() {
        let label = format!("{}/{}", fixture.provider, fixture.name);
        let event = normalise(&fixture, CaptureMode::LocalSemantic);
        assert!(
            event.raw.is_some(),
            "{label}: raw retained in local_semantic"
        );
        assert!(
            !privacy::raw_has_transcript_path(&event),
            "{label}: raw has transcript path"
        );
        let serialised = serde_json::to_string(&event).unwrap();
        for needle in [
            "transcript_path",
            "/home/dev/.claude",
            "/home/dev/.gemini",
            ".jsonl",
        ] {
            assert!(
                !serialised.contains(needle),
                "{label}: local envelope contains {needle:?}"
            );
        }
        let raw = event.raw.as_ref().unwrap().as_object().unwrap();
        assert!(
            raw.keys().all(|k| !k.starts_with('_')),
            "{label}: fixture annotations in raw"
        );
    }
}

#[test]
fn canary_fixtures_keep_content_only_in_content() {
    let fixtures: Vec<Fixture> = load_fixtures()
        .into_iter()
        .filter(|f| f.name.starts_with("canary_"))
        .collect();
    assert!(
        fixtures.len() >= 6,
        "expected canary fixtures for every provider"
    );
    for fixture in fixtures {
        let label = format!("{}/{}", fixture.provider, fixture.name);
        let event = normalise(&fixture, CaptureMode::LocalSemantic);
        let content = serde_json::to_string(&event.content).unwrap();
        let payload_canaries = privacy::collect_content_strings(&fixture.payload);
        assert!(
            privacy::find_leak(&content, &payload_canaries).is_some(),
            "{label}: local_semantic content should retain the payload content"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. Unknown events
// ---------------------------------------------------------------------------

#[test]
fn unrecognised_event_names_map_to_unknown() {
    let ctx = context(CaptureMode::LocalSemantic);
    for provider in PROVIDERS {
        let adapter = adapter(provider);
        let payload = serde_json::json!({
            "hook_event_name": "TotallyNewEvent",
            "session_id": "s-1",
            "conversation_id": "s-1",
            "cwd": PROJECT_ROOT,
            "prompt": "should not be classified"
        });
        let event = adapter.normalise(&ctx, None, &payload).unwrap();
        assert_eq!(event.kind, EventKind::Unknown, "{provider}");
        assert_eq!(event.provider_event_name, "TotallyNewEvent", "{provider}");
        assert_eq!(
            event.attrs.get("hook_event_name"),
            Some(&Value::from("TotallyNewEvent"))
        );
        assert!(!adapter.supported_events().contains(&"TotallyNewEvent"));

        // The hint fills in when the payload has no name.
        let hinted = adapter
            .normalise(
                &ctx,
                Some("AnotherNewEvent"),
                &serde_json::json!({"session_id": "s-1"}),
            )
            .unwrap();
        assert_eq!(hinted.kind, EventKind::Unknown);
        assert_eq!(hinted.provider_event_name, "AnotherNewEvent");

        // A supported-but-unmapped Claude Code event is also Unknown, not an error.
        if *provider == "claude_code" {
            let setup = adapter
                .normalise(
                    &ctx,
                    None,
                    &serde_json::json!({"hook_event_name": "Setup", "session_id": "s"}),
                )
                .unwrap();
            assert_eq!(setup.kind, EventKind::Unknown);
            assert!(adapter.supported_events().contains(&"Setup"));
        }
    }
}

#[test]
fn malformed_payloads_are_errors_not_panics() {
    let ctx = context(CaptureMode::LocalSemantic);
    for provider in PROVIDERS {
        let adapter = adapter(provider);
        assert!(matches!(
            adapter.normalise(&ctx, None, &serde_json::json!({"session_id": "x"})),
            Err(AdapterError::MissingEventName)
        ));
        assert!(matches!(
            adapter.normalise(&ctx, Some("PostToolUse"), &Value::Array(vec![])),
            Err(AdapterError::PayloadNotObject)
        ));
        // Wrong types everywhere: still an event, never a panic.
        let odd = serde_json::json!({
            "hook_event_name": "PostToolUse",
            "session_id": 42,
            "tool_name": ["Bash"],
            "tool_input": "not an object",
            "tool_response": null,
            "duration_ms": "fast",
            "timestamp": "not a date",
            "edits": 7
        });
        let event = adapter.normalise(&ctx, None, &odd).unwrap();
        assert_eq!(event.provider_session_id, "unknown", "{provider}");
        assert_eq!(event.observed_at, CAPTURED_AT, "{provider}");
        assert!(event.duration_ms.is_none(), "{provider}");
    }
}

// ---------------------------------------------------------------------------
// 5. Session identity
// ---------------------------------------------------------------------------

#[test]
fn session_ids_are_deterministic_and_provider_scoped() {
    let ctx = context(CaptureMode::MetadataOnly);
    let claude = adapter("claude_code");
    let codex = adapter("codex");
    let a = claude
        .normalise(
            &ctx,
            None,
            &serde_json::json!({"hook_event_name": "SessionStart", "session_id": "abc"}),
        )
        .unwrap();
    let b = claude
        .normalise(
            &ctx,
            None,
            &serde_json::json!({"hook_event_name": "Stop", "session_id": "abc"}),
        )
        .unwrap();
    let c = codex
        .normalise(
            &ctx,
            None,
            &serde_json::json!({"hook_event_name": "Stop", "session_id": "abc"}),
        )
        .unwrap();
    assert_eq!(a.session_id, b.session_id);
    assert_ne!(
        a.session_id, c.session_id,
        "same provider session id, different providers"
    );
    assert_eq!(a.agent.agent_id, b.agent.agent_id);
    assert_ne!(a.event_id, b.event_id);

    // Cursor keys on conversation_id, which is stable across the conversation.
    let cursor = adapter("cursor");
    let start = cursor
        .normalise(&ctx, None, &serde_json::json!({"hook_event_name": "sessionStart", "conversation_id": "conv-1", "session_id": "sess-9"}))
        .unwrap();
    let edit = cursor
        .normalise(&ctx, None, &serde_json::json!({"hook_event_name": "afterFileEdit", "conversation_id": "conv-1", "file_path": "/home/dev/example/project/a.ts", "edits": []}))
        .unwrap();
    let only_session = cursor
        .normalise(
            &ctx,
            None,
            &serde_json::json!({"hook_event_name": "sessionEnd", "session_id": "sess-9"}),
        )
        .unwrap();
    assert_eq!(start.provider_session_id, "conv-1");
    assert_eq!(start.session_id, edit.session_id);
    assert_eq!(only_session.provider_session_id, "sess-9");

    // The fixture set agrees: every fixture for a provider session shares one canonical id.
    let fixtures = load_fixtures();
    let gemini: Vec<Event> = fixtures
        .iter()
        .filter(|f| f.provider == "gemini_cli" && f.payload["session_id"] == "sess-gem-2")
        .map(|f| normalise(f, CaptureMode::MetadataOnly))
        .collect();
    assert!(gemini.len() >= 3);
    assert!(
        gemini
            .windows(2)
            .all(|w| w[0].session_id == w[1].session_id)
    );
}
