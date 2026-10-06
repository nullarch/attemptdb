//! The same real-world action arriving through several channels is stored
//! once: hooks, a transcript or rollout import, a re-import.
//!
//! Tool calls merge by id (hook and importer derive the id from the
//! provider's call id); everything else a hook also saw is reconciled by the
//! importer against what the database holds. These tests drive the real
//! importers, the real ingest and the real adapters over the fixtures; the
//! end-to-end counts through the projection are in
//! `crates/attempt/tests/dedup_channels.rs`.

use super::*;
use crate::import_codex::{collect_rollouts, import_codex_rollouts};
use crate::import_common::{DbSink, ImportTarget, open_import_target};
use crate::locator::Locator;
use attemptdb_adapters::adapter_for;
use attemptdb_adapters::transcript::{TranscriptOptions, parse_claude_transcript};
use attemptdb_core::{CaptureMode, Event, EventKind};
use attemptdb_storage::{Database, OpenOptions, ScanFilter};
use serde_json::{Value, json};

const CLAUDE_SESSION: &str = "11111111-1111-4111-8111-111111111111";
const CODEX_SESSION: &str = "33333333-3333-4333-8333-333333333333";
const REMOTE: &str = "git@github.com:example/project.git";

fn fixture(kind: &str, name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/transcripts")
        .join(kind)
        .join(format!("{name}.jsonl"))
}

fn open_db(root: &Path) -> (Database, DeviceId) {
    let device = DeviceId::derive(&["import-dedup-tests"]);
    let dir = root.join(".attemptdb");
    Database::create(&dir, device).unwrap();
    let db = Database::open(
        &dir,
        OpenOptions {
            create: false,
            ..Default::default()
        },
    )
    .unwrap();
    (db, device)
}

/// `basic_turn` as a transcript file under a projects directory.
fn claude_sources(root: &Path) -> Vec<TranscriptSource> {
    let dir = root.join("projects").join("-home-dev-example-project");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{CLAUDE_SESSION}.jsonl"));
    std::fs::copy(fixture("claude_code", "basic_turn"), &path).unwrap();
    collect_transcripts(&path)
}

fn project(device: &DeviceId) -> ProjectRef {
    ProjectRef::derive("/home/dev/example/project", Some(REMOTE), device)
}

/// One hook event, as the hook process would build it at `at`.
fn hook(
    provider: Provider,
    device: DeviceId,
    mode: CaptureMode,
    at: &str,
    payload: Value,
) -> Event {
    let ctx = CaptureContext {
        device_id: device,
        capture_mode: mode,
        project: project(&device),
        captured_at: Timestamp::parse(at).unwrap(),
        provider_version: None,
        hook_version: Some("0.2.13".into()),
    };
    adapter_for(&provider)
        .unwrap()
        .normalise(&ctx, None, &payload)
        .unwrap()
}

fn claude_hook(device: DeviceId, at: &str, payload: Value) -> Event {
    hook(
        Provider::ClaudeCode,
        device,
        CaptureMode::LocalSemantic,
        at,
        payload,
    )
}

fn pre_tool(call: &str, tool: &str) -> Value {
    json!({
        "hook_event_name": "PreToolUse", "session_id": CLAUDE_SESSION,
        "tool_name": tool, "tool_use_id": call, "tool_input": {"command": "cargo test -p example"}
    })
}

fn post_tool(call: &str, tool: &str) -> Value {
    json!({
        "hook_event_name": "PostToolUse", "session_id": CLAUDE_SESSION,
        "tool_name": tool, "tool_use_id": call, "tool_input": {"command": "cargo test -p example"},
        "tool_response": {"stdout": "ok", "exit_code": 0}
    })
}

/// What hooks saw of the first turn of `basic_turn` (1 prompt, 1 tool call,
/// 1 turn end) plus the session start.
fn hooks_of_turn_one(device: DeviceId) -> Vec<Event> {
    vec![
        claude_hook(
            device,
            "2026-08-20T08:59:30.000Z",
            json!({"hook_event_name": "SessionStart", "session_id": CLAUDE_SESSION, "source": "startup"}),
        ),
        claude_hook(
            device,
            "2026-08-20T09:00:00.300Z",
            json!({"hook_event_name": "UserPromptSubmit", "session_id": CLAUDE_SESSION, "prompt": "hooked prompt one"}),
        ),
        claude_hook(
            device,
            "2026-08-20T09:00:04.100Z",
            pre_tool("toolu_0001", "Bash"),
        ),
        claude_hook(
            device,
            "2026-08-20T09:00:05.050Z",
            post_tool("toolu_0001", "Bash"),
        ),
        claude_hook(
            device,
            "2026-08-20T09:00:09.400Z",
            json!({"hook_event_name": "Stop", "session_id": CLAUDE_SESSION, "stop_hook_active": false}),
        ),
    ]
}

fn kinds_in(db: &Database) -> std::collections::BTreeMap<&'static str, usize> {
    let mut out = std::collections::BTreeMap::new();
    for e in db.scan(&ScanFilter::default()).unwrap() {
        *out.entry(e.kind.as_str()).or_insert(0) += 1;
    }
    out
}

fn count(db: &Database, kind: EventKind) -> usize {
    kinds_in(db).get(kind.as_str()).copied().unwrap_or(0)
}

fn config() -> Config {
    Config::default()
}

// ---------------------------------------------------------------------------
// Claude Code
// ---------------------------------------------------------------------------

#[test]
fn hooks_then_import_stores_each_real_event_once() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut db, device) = open_db(tmp.path());
    let report = db.ingest(hooks_of_turn_one(device)).unwrap();
    assert_eq!(report.accepted, 5);

    let summary =
        import_claude_transcripts(&mut db, &claude_sources(tmp.path()), &config(), device).unwrap();
    // The transcript has 12 events. Hooks already hold: the session start,
    // prompt one, tool call one (start + end) and turn one's end.
    assert_eq!(summary.events_seen, 12);
    assert_eq!(summary.skipped_captured, 5, "{summary:?}");
    assert_eq!(summary.accepted + summary.duplicates, 7);
    assert!(summary.warnings.is_empty(), "{:?}", summary.warnings);

    // Truth: 2 prompts, 2 tool calls, 2 turn ends, 1 session start.
    assert_eq!(count(&db, EventKind::PromptSubmitted), 2);
    assert_eq!(count(&db, EventKind::ToolCallStarted), 2);
    assert_eq!(count(&db, EventKind::ToolCallFinished), 1);
    assert_eq!(count(&db, EventKind::ToolCallFailed), 1);
    assert_eq!(count(&db, EventKind::TurnStopped), 2);
    assert_eq!(count(&db, EventKind::SessionStarted), 1);
    // What hooks never carry is still imported.
    assert_eq!(count(&db, EventKind::AgentMessage), 3);

    // The hook's own events won: they have a hook version and their prompt.
    let events = db.scan(&ScanFilter::default()).unwrap();
    let prompts: Vec<&Event> = events
        .iter()
        .filter(|e| e.kind == EventKind::PromptSubmitted)
        .collect();
    assert!(prompts[0].hook_version.is_some());
    assert_eq!(
        prompts[0].content.as_ref().unwrap().prompt.as_deref(),
        Some("hooked prompt one")
    );
    assert!(prompts[1].hook_version.is_none(), "prompt two is imported");
}

#[test]
fn a_second_import_after_hooks_changes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut db, device) = open_db(tmp.path());
    db.ingest(hooks_of_turn_one(device)).unwrap();
    let sources = claude_sources(tmp.path());
    import_claude_transcripts(&mut db, &sources, &config(), device).unwrap();
    let before = kinds_in(&db);
    let second = import_claude_transcripts(&mut db, &sources, &config(), device).unwrap();
    assert_eq!(second.accepted, 0, "{second:?}");
    assert_eq!(second.skipped_captured, 5);
    assert_eq!(
        second.skipped_captured + second.duplicates,
        second.events_seen
    );
    assert_eq!(kinds_in(&db), before);
}

#[test]
fn import_then_hooks_merges_tool_calls_by_id() {
    // The other arrival order. The transcript is imported first (hooks were
    // not installed yet, or their spool had not been drained); the hook
    // events for the same tool call arrive afterwards.
    let tmp = tempfile::tempdir().unwrap();
    let (mut db, device) = open_db(tmp.path());
    import_claude_transcripts(&mut db, &claude_sources(tmp.path()), &config(), device).unwrap();
    assert_eq!(count(&db, EventKind::ToolCallStarted), 2);

    let late = db
        .ingest(vec![
            claude_hook(
                device,
                "2026-08-20T09:00:04.100Z",
                pre_tool("toolu_0001", "Bash"),
            ),
            claude_hook(
                device,
                "2026-08-20T09:00:05.050Z",
                post_tool("toolu_0001", "Bash"),
            ),
        ])
        .unwrap();
    assert_eq!((late.accepted, late.duplicates), (0, 2), "same ids: merged");
    assert_eq!(count(&db, EventKind::ToolCallStarted), 2);
    assert_eq!(count(&db, EventKind::ToolCallFinished), 1);

    // What has no natural id is not merged this way: a hook prompt that
    // lands after the import of the same prompt is a second prompt. The
    // importer cannot reconcile what arrives after it ran. (An import runs
    // after the hook events of what it reads: a hook fires before the agent
    // writes the entry, and the import drains the hook spool first.)
    db.ingest(vec![claude_hook(
        device,
        "2026-08-20T09:00:00.300Z",
        json!({"hook_event_name": "UserPromptSubmit", "session_id": CLAUDE_SESSION, "prompt": "late"}),
    )])
    .unwrap();
    assert_eq!(count(&db, EventKind::PromptSubmitted), 3);
}

#[test]
fn a_hook_registered_twice_stores_one_event_per_call_event() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut db, device) = open_db(tmp.path());
    let twice = vec![
        claude_hook(
            device,
            "2026-08-20T09:00:04.100Z",
            pre_tool("toolu_0001", "Bash"),
        ),
        claude_hook(
            device,
            "2026-08-20T09:00:04.101Z",
            pre_tool("toolu_0001", "Bash"),
        ),
        claude_hook(
            device,
            "2026-08-20T09:00:05.050Z",
            post_tool("toolu_0001", "Bash"),
        ),
        claude_hook(
            device,
            "2026-08-20T09:00:05.052Z",
            post_tool("toolu_0001", "Bash"),
        ),
    ];
    let report = db.ingest(twice).unwrap();
    assert_eq!((report.accepted, report.duplicates), (2, 2));
}

#[test]
fn prompts_are_matched_by_order_never_by_text() {
    // Hooks were installed after the first prompt: only prompt two was
    // captured, with different text than the transcript's. Prompt one is
    // imported, prompt two is the hook's.
    let tmp = tempfile::tempdir().unwrap();
    let (mut db, device) = open_db(tmp.path());
    db.ingest(vec![claude_hook(
        device,
        "2026-08-20T09:01:00.200Z",
        json!({"hook_event_name": "UserPromptSubmit", "session_id": CLAUDE_SESSION, "prompt": "not the transcript's words"}),
    )])
    .unwrap();
    let summary =
        import_claude_transcripts(&mut db, &claude_sources(tmp.path()), &config(), device).unwrap();
    assert_eq!(summary.skipped_captured, 1);
    let events = db.scan(&ScanFilter::default()).unwrap();
    let prompts: Vec<&Event> = events
        .iter()
        .filter(|e| e.kind == EventKind::PromptSubmitted)
        .collect();
    assert_eq!(prompts.len(), 2);
    let imported = prompts.iter().find(|e| e.hook_version.is_none()).unwrap();
    assert!(
        imported
            .content
            .as_ref()
            .unwrap()
            .prompt
            .as_deref()
            .unwrap()
            .starts_with("CANARY_PROMPT_ONE"),
        "the first prompt was imported, the second stayed the hook's"
    );
}

#[test]
fn a_hook_event_too_far_away_is_not_the_same_event() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut db, device) = open_db(tmp.path());
    // 30 s after the transcript's second prompt: another prompt, not it.
    db.ingest(vec![claude_hook(
        device,
        "2026-08-20T09:01:30.000Z",
        json!({"hook_event_name": "UserPromptSubmit", "session_id": CLAUDE_SESSION, "prompt": "x"}),
    )])
    .unwrap();
    let summary =
        import_claude_transcripts(&mut db, &claude_sources(tmp.path()), &config(), device).unwrap();
    assert_eq!(summary.skipped_captured, 0);
    assert_eq!(count(&db, EventKind::PromptSubmitted), 3);
}

#[test]
fn two_hook_prompts_consume_two_transcript_prompts_and_the_third_is_imported() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut db, device) = open_db(tmp.path());
    // Both prompts captured by hooks; the transcript also holds a queued
    // prompt the hooks missed (a third prompt entry).
    let mut lines: Vec<String> = std::fs::read_to_string(fixture("claude_code", "basic_turn"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    lines.push(
        r#"{"type":"attachment","attachment":{"type":"queued_command","prompt":"CANARY_QUEUED_AT_EOF one more","commandMode":"prompt"},"sessionId":"11111111-1111-4111-8111-111111111111","cwd":"/home/dev/example/project","uuid":"a00000ff-0000-4000-8000-000000000000","timestamp":"2026-08-20T09:02:00.000Z"}"#
            .into(),
    );
    let dir = tmp
        .path()
        .join("projects")
        .join("-home-dev-example-project");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{CLAUDE_SESSION}.jsonl"));
    std::fs::write(&path, lines.join("\n") + "\n").unwrap();
    db.ingest(vec![
        claude_hook(
            device,
            "2026-08-20T09:00:00.300Z",
            json!({"hook_event_name": "UserPromptSubmit", "session_id": CLAUDE_SESSION, "prompt": "a"}),
        ),
        claude_hook(
            device,
            "2026-08-20T09:01:00.300Z",
            json!({"hook_event_name": "UserPromptSubmit", "session_id": CLAUDE_SESSION, "prompt": "b"}),
        ),
    ])
    .unwrap();
    let summary =
        import_claude_transcripts(&mut db, &collect_transcripts(&path), &config(), device).unwrap();
    assert_eq!(summary.skipped_captured, 2);
    assert_eq!(
        count(&db, EventKind::PromptSubmitted),
        3,
        "2 hooks + the queued one"
    );
}

#[test]
fn a_session_hooks_never_saw_is_imported_whole() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut db, device) = open_db(tmp.path());
    // Hooks of ANOTHER session of the same provider are no reason to skip.
    db.ingest(vec![claude_hook(
        device,
        "2026-08-20T09:00:00.300Z",
        json!({"hook_event_name": "UserPromptSubmit", "session_id": "some-other-session", "prompt": "x"}),
    )])
    .unwrap();
    let summary =
        import_claude_transcripts(&mut db, &claude_sources(tmp.path()), &config(), device).unwrap();
    assert_eq!(summary.skipped_captured, 0);
    assert_eq!(summary.accepted, 12);
    assert_eq!(
        count(&db, EventKind::PromptSubmitted),
        3,
        "2 imported + 1 of the other session"
    );
}

#[test]
fn hook_events_from_before_natural_ids_still_stop_the_double_count() {
    // A database captured by an older hook: random event ids for the same
    // tool call. The importer joins on the call id the hook event carries.
    let tmp = tempfile::tempdir().unwrap();
    let (mut db, device) = open_db(tmp.path());
    let mut old = vec![
        claude_hook(
            device,
            "2026-08-20T09:00:04.100Z",
            pre_tool("toolu_0001", "Bash"),
        ),
        claude_hook(
            device,
            "2026-08-20T09:00:05.050Z",
            post_tool("toolu_0001", "Bash"),
        ),
    ];
    for ev in &mut old {
        ev.event_id = attemptdb_core::EventId::new();
    }
    db.ingest(old).unwrap();
    let summary =
        import_claude_transcripts(&mut db, &claude_sources(tmp.path()), &config(), device).unwrap();
    assert_eq!(summary.skipped_captured, 2);
    assert_eq!(count(&db, EventKind::ToolCallStarted), 2);
    assert_eq!(count(&db, EventKind::ToolCallFinished), 1);
}

#[test]
fn tool_calls_stored_by_an_older_import_are_not_stored_again() {
    // What an earlier release stored: tool events under the id derived from
    // `(session, entry uuid, block)`.
    let tmp = tempfile::tempdir().unwrap();
    let (mut db, device) = open_db(tmp.path());
    let sources = claude_sources(tmp.path());
    let ctx = CaptureContext {
        device_id: device,
        capture_mode: CaptureMode::LocalSemantic,
        project: ProjectRef::derive("/home/dev/example/project", None, &device),
        captured_at: Timestamp::now(),
        provider_version: None,
        hook_version: None,
    };
    let parsed = parse_claude_transcript(
        std::fs::read_to_string(&sources[0].path)
            .unwrap()
            .lines()
            .map(str::to_string),
        &ctx,
        &TranscriptOptions::default(),
    );
    let events: Vec<Event> = parsed
        .events
        .into_iter()
        .zip(parsed.legacy_ids)
        .map(|(mut ev, legacy)| {
            if let Some(old) = legacy {
                ev.event_id = old;
            }
            ev
        })
        .collect();
    let stored = events.len();
    assert_eq!(db.ingest(events).unwrap().accepted, stored);

    let summary = import_claude_transcripts(&mut db, &sources, &config(), device).unwrap();
    assert_eq!(summary.accepted, 0, "nothing new: {summary:?}");
    assert_eq!(summary.skipped_captured, 4, "the four tool events");
    assert_eq!(summary.duplicates, stored - 4);
    assert_eq!(db.scan(&ScanFilter::default()).unwrap().len(), stored);
}

#[test]
fn the_project_of_a_session_is_the_one_its_hooks_carry() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut db, device) = open_db(tmp.path());
    // The checkout is gone from this machine and the transcript has no remote:
    // its own derivation is a hash of the path. The hooks computed the
    // remote-based identity while the checkout existed.
    let hooked = project(&device);
    db.ingest(hooks_of_turn_one(device)).unwrap();
    import_claude_transcripts(&mut db, &claude_sources(tmp.path()), &config(), device).unwrap();
    let events = db.scan(&ScanFilter::default()).unwrap();
    assert!(!events.is_empty());
    for e in &events {
        assert_eq!(e.project.project_id, hooked.project_id, "{:?}", e.kind);
        assert_eq!(e.project.name, hooked.name);
        assert_eq!(e.project.repo_remote, hooked.repo_remote);
    }
    // The branch is the transcript's, taken at the time.
    let imported = events.iter().find(|e| e.hook_version.is_none()).unwrap();
    assert_eq!(imported.project.branch.as_deref(), Some("main"));

    // Without hook events the fallback is unchanged.
    let tmp2 = tempfile::tempdir().unwrap();
    let (mut db2, device2) = open_db(tmp2.path());
    import_claude_transcripts(&mut db2, &claude_sources(tmp2.path()), &config(), device2).unwrap();
    let alone = db2.scan(&ScanFilter::default()).unwrap();
    let fallback = ProjectRef::derive("/home/dev/example/project", None, &device2);
    assert!(
        alone
            .iter()
            .all(|e| e.project.project_id == fallback.project_id)
    );
    assert_ne!(fallback.project_id, hooked.project_id);
}

#[test]
fn reconciliation_needs_no_text_metadata_only_matches_the_same_way() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut db, device) = open_db(tmp.path());
    let blind = Config {
        capture_mode: CaptureMode::MetadataOnly,
        ..Config::default()
    };
    let mut hooks = hooks_of_turn_one(device);
    // Rebuild the hook events under metadata_only: no content, no raw.
    hooks = hooks
        .into_iter()
        .map(|ev| {
            let mut ev = ev;
            ev.capture_mode = CaptureMode::MetadataOnly;
            ev.apply_capture_mode();
            ev
        })
        .collect();
    db.ingest(hooks).unwrap();
    let summary =
        import_claude_transcripts(&mut db, &claude_sources(tmp.path()), &blind, device).unwrap();
    assert_eq!(summary.skipped_captured, 5);
    assert_eq!(count(&db, EventKind::PromptSubmitted), 2);
    let serialised = serde_json::to_string(&db.scan(&ScanFilter::default()).unwrap()).unwrap();
    assert!(!serialised.contains("CANARY_"));
}

#[test]
fn an_interrupted_turn_has_no_hook_stop_and_never_matches_one() {
    // `interrupted_turn` ends turns with the person pressing Escape: Claude
    // fires no Stop hook for those, so a Stop hook close in time belongs to
    // another turn.
    let tmp = tempfile::tempdir().unwrap();
    let (mut db, device) = open_db(tmp.path());
    let dir = tmp
        .path()
        .join("projects")
        .join("-home-dev-example-project");
    std::fs::create_dir_all(&dir).unwrap();
    let sid = "22222222-2222-4222-8222-222222222222";
    let path = dir.join(format!("{sid}.jsonl"));
    std::fs::copy(fixture("claude_code", "interrupted_turn"), &path).unwrap();
    let parsed = {
        let ctx = CaptureContext {
            device_id: device,
            capture_mode: CaptureMode::LocalSemantic,
            project: project(&device),
            captured_at: Timestamp::now(),
            provider_version: None,
            hook_version: None,
        };
        parse_claude_transcript(
            std::fs::read_to_string(&path)
                .unwrap()
                .lines()
                .map(str::to_string),
            &ctx,
            &TranscriptOptions {
                session_id_hint: Some(sid.into()),
                ..TranscriptOptions::default()
            },
        )
    };
    let session = parsed.provider_session_id.clone().unwrap();
    let interrupt = parsed
        .events
        .iter()
        .find(|e| e.provider_event_name == "transcript:user:interrupted")
        .unwrap();
    let at = interrupt.observed_at;
    let stop_at = Timestamp::from_micros(at.as_micros() + 2_000_000).to_rfc3339();
    db.ingest(vec![claude_hook(
        device,
        &stop_at,
        json!({"hook_event_name": "Stop", "session_id": session, "stop_hook_active": false}),
    )])
    .unwrap();
    let stops_before = count(&db, EventKind::TurnStopped);
    let summary =
        import_claude_transcripts(&mut db, &collect_transcripts(&path), &config(), device).unwrap();
    // The transcript's turn ends are all imported except the one the hook
    // matches (the synthesised end of the last turn, within tolerance only
    // if close; here the interruption two seconds from the hook is not it).
    let imported_stops = count(&db, EventKind::TurnStopped) - stops_before;
    let transcript_stops = parsed
        .events
        .iter()
        .filter(|e| e.kind == EventKind::TurnStopped)
        .count();
    assert_eq!(imported_stops + summary.skipped_captured, transcript_stops);
    assert!(
        db.scan(&ScanFilter::default())
            .unwrap()
            .iter()
            .any(|e| { e.provider_event_name == "transcript:user:interrupted" }),
        "interruptions are always imported"
    );
}

// ---------------------------------------------------------------------------
// The daemon holds the database
// ---------------------------------------------------------------------------

#[test]
fn through_the_spool_the_lookup_still_sees_what_the_daemon_stored() {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    let db_dir = tmp.path().join("db");
    std::fs::create_dir_all(&data).unwrap();
    let device = DeviceId::derive(&["import-dedup-spool"]);
    Database::create(&db_dir, device).unwrap();
    let locator = Locator::resolve(tmp.path(), Some(&data), Some(&db_dir));
    let sources = claude_sources(tmp.path());

    // The "daemon": a writer holding the lock, with the hook events already
    // acknowledged (in its WAL; a read-only open replays it).
    let mut holder = crate::ingest::open_writer(&locator, false).unwrap();
    holder.ingest(hooks_of_turn_one(device)).unwrap();
    let ImportTarget::Spool(mut spool) = open_import_target(&locator).unwrap() else {
        panic!("the lock is held: the spool is the target")
    };
    let queued = import_claude_transcripts_to(&mut spool, &sources, &config(), device).unwrap();
    assert_eq!(queued.skipped_captured, 5, "{queued:?}");
    assert_eq!(queued.queued, 7, "only what hooks lack is queued");
    assert!(queued.warnings.is_empty(), "{:?}", queued.warnings);

    let report = holder.import_spool().unwrap();
    assert_eq!(report.accepted, 7);
    assert_eq!(count(&holder, EventKind::PromptSubmitted), 2);
    assert_eq!(count(&holder, EventKind::ToolCallStarted), 2);
}

// ---------------------------------------------------------------------------
// Codex
// ---------------------------------------------------------------------------

fn codex_hook(device: DeviceId, at: &str, payload: Value) -> Event {
    hook(
        Provider::Codex,
        device,
        CaptureMode::LocalSemantic,
        at,
        payload,
    )
}

fn codex_rollout(root: &Path) -> Vec<crate::import_codex::RolloutSource> {
    let path = root.join("rollout-2026-08-29T08-00-00-33333333-3333-4333-8333-333333333333.jsonl");
    std::fs::copy(fixture("codex", "classic_turn"), &path).unwrap();
    collect_rollouts(&path)
}

fn codex_hooks(device: DeviceId) -> Vec<Event> {
    vec![
        codex_hook(
            device,
            "2026-08-29T08:00:02.100Z",
            json!({"hook_event_name": "UserPromptSubmit", "session_id": CODEX_SESSION, "turn_id": "t1", "prompt": "p"}),
        ),
        codex_hook(
            device,
            "2026-08-29T08:00:05.700Z",
            json!({
                "hook_event_name": "PreToolUse", "session_id": CODEX_SESSION, "turn_id": "t1",
                "tool_name": "exec_command", "tool_use_id": "call_ec01",
                "tool_input": {"cmd": "git status --short"}
            }),
        ),
        codex_hook(
            device,
            "2026-08-29T08:00:05.900Z",
            json!({
                "hook_event_name": "PostToolUse", "session_id": CODEX_SESSION, "turn_id": "t1",
                "tool_name": "exec_command", "tool_use_id": "call_ec01",
                "tool_input": {"cmd": "git status --short"},
                "tool_response": {"output": "Process exited with code 0"}
            }),
        ),
        codex_hook(
            device,
            "2026-08-29T08:00:25.200Z",
            json!({"hook_event_name": "Stop", "session_id": CODEX_SESSION, "turn_id": "t1", "stop_hook_active": false}),
        ),
    ]
}

#[test]
fn codex_hooks_then_rollout_import_stores_each_real_event_once() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut db, device) = open_db(tmp.path());
    db.ingest(codex_hooks(device)).unwrap();
    let rollout_only = {
        // The same rollout into an empty database, for the totals.
        let tmp2 = tempfile::tempdir().unwrap();
        let (mut db2, device2) = open_db(tmp2.path());
        import_codex_rollouts(
            &mut DbSink::new(&mut db2),
            &codex_rollout(tmp2.path()),
            &config(),
            device2,
        )
        .unwrap()
    };
    let summary = import_codex_rollouts(
        &mut DbSink::new(&mut db),
        &codex_rollout(tmp.path()),
        &config(),
        device,
    )
    .unwrap();
    assert_eq!(summary.events_seen, rollout_only.events_seen);
    // The hooks hold the prompt, call_ec01's start and end, and the turn end.
    assert_eq!(summary.skipped_captured, 4, "{summary:?}");
    assert_eq!(count(&db, EventKind::PromptSubmitted), 1);
    assert_eq!(count(&db, EventKind::TurnStopped), 1);
    let call_ec01 = db
        .scan(&ScanFilter::default())
        .unwrap()
        .into_iter()
        .filter(|e| e.tool.as_ref().and_then(|t| t.call_id.as_deref()) == Some("call_ec01"))
        .count();
    assert_eq!(call_ec01, 2, "one start, one end");
    // Run again: nothing moves.
    let again = import_codex_rollouts(
        &mut DbSink::new(&mut db),
        &codex_rollout(tmp.path()),
        &config(),
        device,
    )
    .unwrap();
    assert_eq!(again.accepted, 0);
}

#[test]
fn codex_rollout_import_then_hooks_merges_tool_calls_by_id() {
    let tmp = tempfile::tempdir().unwrap();
    let (mut db, device) = open_db(tmp.path());
    import_codex_rollouts(
        &mut DbSink::new(&mut db),
        &codex_rollout(tmp.path()),
        &config(),
        device,
    )
    .unwrap();
    let late = db.ingest(codex_hooks(device)).unwrap();
    // Only the two tool events share an id with the rollout's.
    assert_eq!(late.duplicates, 2, "{late:?}");
    assert_eq!(
        late.accepted, 2,
        "the prompt and the stop have no natural id"
    );
}
