//! One session seen through several channels projects to what happened, not
//! to what each channel saw.
//!
//! REPORT.md §5.14's probe: a session whose hooks captured 1 prompt, 1 tool
//! call and 1 turn, and whose transcript holds 2 / 2 / 2, projected to 3
//! prompts, 4 tool calls and 3 turns, with unpaired starts and ends. The truth
//! is 2 / 2 / 2. These tests build that scenario end to end: real adapters
//! produce the hook events, they reach the database through the spool and the
//! real ingest, the real importer reads the transcript fixture, and the real
//! projection counts. No CLI binary runs and nothing reads an agent's config
//! directory: the importers are handed explicit files.
//!
//! The ways a channel can still double count are asserted too, so that the
//! documentation (`docs/history-import.md`) and the code cannot drift apart.

use attemptdb_adapters::transcript::{TranscriptOptions, parse_claude_transcript};
use attemptdb_adapters::{CaptureContext, adapter_for};
use attemptdb_capture::config::Config;
use attemptdb_capture::import::{TranscriptSource, collect_transcripts, import_claude_transcripts};
use attemptdb_core::event::Provider;
use attemptdb_core::{CaptureMode, DeviceId, Event, EventId, EventKind, ProjectRef, Timestamp};
use attemptdb_project::project;
use attemptdb_storage::{Database, OpenOptions, ScanFilter, SpoolWriter};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const SESSION: &str = "11111111-1111-4111-8111-111111111111";

struct World {
    tmp: tempfile::TempDir,
    db: Database,
    device: DeviceId,
}

fn world() -> World {
    let tmp = tempfile::tempdir().unwrap();
    let device = DeviceId::derive(&["dedup-channels"]);
    let dir = tmp.path().join(".attemptdb");
    Database::create(&dir, device).unwrap();
    let db = Database::open(
        &dir,
        OpenOptions {
            create: false,
            ..Default::default()
        },
    )
    .unwrap();
    World { tmp, db, device }
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/transcripts/claude_code/basic_turn.jsonl")
}

impl World {
    fn transcript(&self) -> Vec<TranscriptSource> {
        let dir = self
            .tmp
            .path()
            .join("projects")
            .join("-home-dev-example-project");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{SESSION}.jsonl"));
        std::fs::copy(fixture(), &path).unwrap();
        collect_transcripts(&path)
    }

    fn import(&mut self) -> attemptdb_capture::import::ImportSummary {
        let sources = self.transcript();
        import_claude_transcripts(&mut self.db, &sources, &Config::default(), self.device).unwrap()
    }

    /// What a hook does: the event is appended to the spool; whoever holds
    /// the database imports it.
    fn hook(&mut self, events: Vec<Event>) {
        SpoolWriter::new(self.db.root())
            .unwrap()
            .append_with(&events, false)
            .unwrap();
        self.db.import_spool().unwrap();
    }

    fn events(&self) -> Vec<Event> {
        self.db.scan(&ScanFilter::default()).unwrap()
    }

    fn counts(&self) -> Counts {
        counts(&self.events())
    }
}

fn hook_event(device: DeviceId, at: &str, payload: Value) -> Event {
    let ctx = CaptureContext {
        device_id: device,
        capture_mode: CaptureMode::LocalSemantic,
        project: ProjectRef::derive(
            "/home/dev/example/project",
            Some("git@github.com:example/project.git"),
            &device,
        ),
        captured_at: Timestamp::parse(at).unwrap(),
        provider_version: None,
        hook_version: Some("0.2.13".into()),
    };
    adapter_for(&Provider::ClaudeCode)
        .unwrap()
        .normalise(&ctx, None, &payload)
        .unwrap()
}

/// What the hooks of the first turn saw: 1 prompt, 1 tool call, 1 turn end.
fn hooks_of_turn_one(device: DeviceId) -> Vec<Event> {
    let tool = |name: &str, response: Option<Value>| {
        let mut p = json!({
            "hook_event_name": name, "session_id": SESSION,
            "tool_name": "Bash", "tool_use_id": "toolu_0001",
            "tool_input": {"command": "cargo test -p example"}
        });
        if let Some(r) = response {
            p["tool_response"] = r;
        }
        p
    };
    vec![
        hook_event(
            device,
            "2026-08-20T09:00:00.300Z",
            json!({"hook_event_name": "UserPromptSubmit", "session_id": SESSION, "prompt": "run the tests"}),
        ),
        hook_event(device, "2026-08-20T09:00:04.100Z", tool("PreToolUse", None)),
        hook_event(
            device,
            "2026-08-20T09:00:05.050Z",
            tool("PostToolUse", Some(json!({"stdout": "ok", "exit_code": 0}))),
        ),
        hook_event(
            device,
            "2026-08-20T09:00:09.400Z",
            json!({"hook_event_name": "Stop", "session_id": SESSION, "stop_hook_active": false}),
        ),
    ]
}

#[derive(Debug, PartialEq, Eq)]
struct Counts {
    sessions: usize,
    prompts: usize,
    turns: usize,
    tool_calls: usize,
    /// Calls with a start and no end, or an end and no start.
    unpaired: usize,
}

fn counts(events: &[Event]) -> Counts {
    let p = project(events);
    Counts {
        sessions: p.sessions.len(),
        prompts: p.sessions.iter().map(|s| s.prompt_count as usize).sum(),
        turns: p.turns.iter().filter(|t| t.index > 0).count(),
        tool_calls: p.tool_calls.len(),
        unpaired: p
            .tool_calls
            .iter()
            .filter(|c| c.started_at.is_none() || c.finished_at.is_none())
            .count(),
    }
}

const TRUTH: Counts = Counts {
    sessions: 1,
    prompts: 2,
    turns: 2,
    tool_calls: 2,
    unpaired: 0,
};

/// Reproduce the §5.14 probe as it behaved before this change: hook events
/// with random ids, transcript events under the ids earlier releases
/// derived, everything written, nothing reconciled.
fn the_old_behaviour(w: &mut World) -> Counts {
    let mut hooks = hooks_of_turn_one(w.device);
    for ev in &mut hooks {
        ev.event_id = EventId::new();
    }
    w.db.ingest(hooks).unwrap();
    let sources = w.transcript();
    let ctx = CaptureContext {
        device_id: w.device,
        capture_mode: CaptureMode::LocalSemantic,
        project: ProjectRef::derive("/home/dev/example/project", None, &w.device),
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
    // The earlier parser neither emitted narration nor named tool events by
    // their call id.
    let events: Vec<Event> = parsed
        .events
        .into_iter()
        .zip(parsed.legacy_ids)
        .filter(|(ev, _)| ev.kind != EventKind::AgentMessage)
        .map(|(mut ev, legacy)| {
            if let Some(old) = legacy {
                ev.event_id = old;
            }
            ev
        })
        .collect();
    w.db.ingest(events).unwrap();
    w.counts()
}

#[test]
fn the_probe_before_the_change_double_counted() {
    let mut w = world();
    let before = the_old_behaviour(&mut w);
    // The report's numbers: prompts 3 and turns 3 where 2 and 2 are true, and
    // tool calls with a start and no end (or the reverse).
    assert_eq!(before.prompts, 3, "{before:?}");
    assert_eq!(before.turns, 3, "{before:?}");
    assert!(before.tool_calls > TRUTH.tool_calls, "{before:?}");
    assert!(before.unpaired > 0, "{before:?}");
}

#[test]
fn hooks_then_transcript_projects_to_the_truth() {
    let mut w = world();
    let device = w.device;
    w.hook(hooks_of_turn_one(device));
    assert_eq!(
        w.counts(),
        Counts {
            sessions: 1,
            prompts: 1,
            turns: 1,
            tool_calls: 1,
            unpaired: 0
        },
        "what the hooks alone saw"
    );
    let summary = w.import();
    assert!(summary.skipped_captured >= 4, "{summary:?}");
    assert_eq!(w.counts(), TRUTH);

    // And again: nothing moves.
    let again = w.import();
    assert_eq!(again.accepted, 0);
    assert_eq!(w.counts(), TRUTH);
}

#[test]
fn a_session_hooks_never_saw_projects_to_the_truth_from_the_transcript_alone() {
    let mut w = world();
    let summary = w.import();
    assert_eq!(summary.skipped_captured, 0);
    assert_eq!(w.counts(), TRUTH);
}

#[test]
fn transcript_then_hooks_merges_the_tool_calls() {
    let mut w = world();
    w.import();
    assert_eq!(w.counts(), TRUTH);
    let device = w.device;
    w.hook(hooks_of_turn_one(device));
    let c = w.counts();
    // The tool call has one id in both channels: still two calls, all paired.
    assert_eq!((c.tool_calls, c.unpaired), (2, 0), "{c:?}");
    // A prompt and a turn end have no id the hook can share with the
    // transcript, and the importer ran before the hook events existed: the
    // overlap is counted twice. Documented in docs/history-import.md; an
    // import runs after the hook events of what it reads, so this order is
    // the exception (hooks installed mid-turn).
    assert_eq!((c.prompts, c.turns), (3, 3), "{c:?}");
}

#[test]
fn a_hook_registered_twice_does_not_double_the_tool_calls() {
    let mut w = world();
    let device = w.device;
    let mut doubled = hooks_of_turn_one(device);
    let echoes: Vec<Event> = doubled
        .iter()
        .filter(|e| {
            matches!(
                e.kind,
                EventKind::ToolCallStarted | EventKind::ToolCallFinished
            )
        })
        .cloned()
        .collect();
    // The second registration's hook process builds its own event for the
    // same payload.
    let echoes: Vec<Event> = echoes
        .into_iter()
        .map(|e| {
            let payload = e.raw.clone().unwrap();
            hook_event(device, "2026-08-20T09:00:04.102Z", payload)
        })
        .collect();
    doubled.extend(echoes);
    w.hook(doubled);
    let c = w.counts();
    assert_eq!((c.tool_calls, c.unpaired), (1, 0), "{c:?}");
    // Same story with the transcript on top.
    w.import();
    assert_eq!(w.counts(), TRUTH);
}
