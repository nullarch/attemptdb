//! Legacy VibeMon envelope (v2) fixtures → canonical events.
//!
//! Fixtures under `fixtures/vibemon-envelope-v2/` are sanitised copies of
//! the `vibemon-hooks` contract goldens (`/home/dev/proj`, `example/project`).

use attemptdb_adapters::vibemon::normalise_envelope;
use attemptdb_core::conformance;
use attemptdb_core::event::Provider;
use attemptdb_core::{CaptureMode, DeviceId, Event, EventKind};
use serde_json::Value;
use std::path::PathBuf;

fn fixtures() -> Vec<(String, Value)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/vibemon-envelope-v2");
    let mut out: Vec<(String, Value)> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .map(|p| {
            let name = p.file_stem().unwrap().to_string_lossy().to_string();
            let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
            (name, v)
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(out.len() >= 6, "fixtures missing under {}", dir.display());
    out
}

fn device() -> DeviceId {
    DeviceId::derive(&["vibemon-test", "device"])
}

#[test]
fn every_fixture_normalises_to_the_expected_kind() {
    for (name, envelope) in fixtures() {
        let ev = normalise_envelope(device(), CaptureMode::LocalSemantic, &envelope)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        let expected = match name.split('_').next().unwrap() {
            "activity" | "bash" => EventKind::ToolCallFinished,
            "prompt" => EventKind::PromptSubmitted,
            "session" if name.contains("start") => EventKind::SessionStarted,
            "session" => EventKind::SessionEnded,
            "tool" => EventKind::ToolCallFailed,
            "stop" | "cursor" => EventKind::TurnStopped,
            other => panic!("no expectation for fixture prefix {other}"),
        };
        assert_eq!(ev.kind, expected, "{name}");
        assert_eq!(ev.hook_version.as_deref(), Some("vibemon-envelope-v2"));
        assert_eq!(ev.attrs["hook_event_name"], envelope["event"]);
        assert!(ev.session_id != attemptdb_core::SessionId::nil());
        assert!(!ev.event_id.is_nil());
        // attrs never carry a home path: cwd is elided.
        for (k, v) in &ev.attrs {
            if let Some(s) = v.as_str() {
                assert!(!s.contains("/home/"), "{name}: attrs.{k} = {s}");
            }
        }
    }
}

#[test]
fn spot_checks() {
    let all: Vec<(String, Event)> = fixtures()
        .into_iter()
        .map(|(n, e)| {
            (
                n,
                normalise_envelope(device(), CaptureMode::LocalSemantic, &e).unwrap(),
            )
        })
        .collect();
    let find = |prefix: &str| {
        all.iter()
            .find(|(n, _)| n.starts_with(prefix))
            .map(|(_, e)| e.clone())
            .unwrap_or_else(|| panic!("fixture {prefix}*"))
    };

    let edit = find("activity_edit");
    assert_eq!(edit.tool.as_ref().unwrap().name, "Edit");
    assert_eq!(edit.attrs["file_ext"], "tsx");
    assert_eq!(edit.duration_ms, Some(1250));
    assert_eq!(edit.paths.len(), 1);
    assert_eq!(edit.attrs["cwd"], "~/proj");
    assert_eq!(edit.project.name, "example/project");

    let commit = find("bash_git_commit");
    assert_eq!(commit.tool.as_ref().unwrap().name, "Bash");
    assert_eq!(commit.attrs["command_category"], "git");
    assert_eq!(commit.attrs["command_subcategory"], "git.commit");
    assert_eq!(
        commit.content.as_ref().and_then(|c| c.message.as_deref()),
        Some("feat: add settlement endpoint"),
        "commit title is content, kept under local_semantic"
    );
    let commit_meta = normalise_envelope(
        device(),
        CaptureMode::MetadataOnly,
        &fixtures()
            .into_iter()
            .find(|(n, _)| n.starts_with("bash_git_commit"))
            .unwrap()
            .1,
    )
    .unwrap();
    assert!(
        commit_meta.content.is_none(),
        "and gone under metadata_only"
    );
    assert!(commit_meta.raw.is_none());

    let prompt = find("prompt");
    assert_eq!(prompt.attrs["prompt_chars"], 42);
    assert_eq!(prompt.attrs["prompt_has_question"], true);
    assert!(
        prompt.content.is_none(),
        "the envelope never carried the prompt body"
    );

    let failure = find("tool_failure");
    assert_eq!(
        failure.outcome.as_ref().unwrap().class.as_deref(),
        Some("string_mismatch")
    );
    assert_eq!(failure.attrs["error_class"], "string_mismatch");

    let start = find("session_start");
    assert_eq!(start.agent.model.as_deref(), Some("claude-sonnet-4-6"));
    assert_eq!(start.attrs["source"], "startup");

    let stop = find("cursor_stop");
    assert_eq!(stop.provider, Provider::Cursor);
}

#[test]
fn normalised_envelopes_are_conformant() {
    let events: Vec<Event> = fixtures()
        .into_iter()
        .map(|(_, e)| normalise_envelope(device(), CaptureMode::MetadataOnly, &e).unwrap())
        .collect();
    let report = conformance::check_parsed(&events);
    assert!(
        report.compatible(),
        "{}",
        serde_json::to_string_pretty(&report).unwrap()
    );
}
