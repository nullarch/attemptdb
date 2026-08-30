//! Legacy input: the VibeMon hook envelope (v2) → canonical [`Event`].
//!
//! `vibemon-hooks` sanitises on the device and sends a content-free envelope
//! (`event`, `agent`, `session_id`, `cwd`, `project_root`, `repo_identifier`,
//! `timestamp`, `payload`, `signals`). Installs that have not moved to
//! `attempt hook` keep working through this adapter; nothing new produces
//! the format. The envelope has no event id, so the id is minted here — the
//! legacy client never retries, which is why that is safe.
//!
//! The one content-bearing signal, `commit.message`, goes to
//! `content.message` and therefore does not survive `metadata_only`.

use crate::common::{classify_tool, file_facts};
use crate::{ADAPTER_VERSION, AdapterError};
use attemptdb_core::event::{EventContent, EventKind, Outcome, Provider, ToolRef};
use attemptdb_core::{
    CaptureMode, DeviceId, Event, PortablePath, ProjectRef, Timestamp, elide_home,
};
use serde_json::{Map, Value, json};

pub const ENVELOPE_VERSION: u64 = 2;

/// The `agent` field, in VibeMon's spelling.
pub fn provider_of(agent: &str) -> Provider {
    match agent {
        "claude_code" => Provider::ClaudeCode,
        "codex_cli" | "codex" => Provider::Codex,
        "cursor" => Provider::Cursor,
        "gemini_cli" => Provider::GeminiCli,
        other => Provider::Other(other.to_string()),
    }
}

/// Coarse category (what projections key on) for a vibemon-hooks
/// `bash.category` value.
fn coarse_of(fine: &str) -> &'static str {
    match fine {
        f if f.starts_with("git.") => "git",
        "pkg.test" | "test.run" => "test",
        "pkg.build" | "build.sys" => "build",
        "pkg.install" | "pkg.system" => "install",
        f if f.starts_with("net.") => "network",
        f if f.starts_with("fs.") => "fs",
        "pkg.run" | "runtime" => "run",
        _ => "other",
    }
}

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|x| !x.is_empty())
}
fn n(v: &Value, key: &str) -> Option<u64> {
    v.get(key).and_then(Value::as_u64)
}
fn b(v: &Value, key: &str) -> Option<bool> {
    v.get(key).and_then(Value::as_bool)
}

/// Normalise one envelope. `device_id` identifies the uploading device
/// (from the bearer key on a server); `capture_mode` is applied afterwards.
pub fn normalise_envelope(
    device_id: DeviceId,
    capture_mode: CaptureMode,
    envelope: &Value,
) -> Result<Event, AdapterError> {
    let Some(obj) = envelope.as_object() else {
        return Err(AdapterError::PayloadNotObject);
    };
    let version = n(envelope, "v").unwrap_or(0);
    if version != ENVELOPE_VERSION {
        return Err(AdapterError::Invalid(format!(
            "envelope v{version}; this adapter reads v{ENVELOPE_VERSION}"
        )));
    }
    let event_name = s(envelope, "event").ok_or(AdapterError::MissingEventName)?;
    let agent =
        s(envelope, "agent").ok_or_else(|| AdapterError::Invalid("agent missing".into()))?;
    let provider = provider_of(agent);
    let payload = obj
        .get("payload")
        .cloned()
        .unwrap_or(Value::Object(Map::new()));
    let signals = obj
        .get("signals")
        .cloned()
        .unwrap_or(Value::Object(Map::new()));

    // Project: the working directory is the root; the repo identifier (or a
    // project_root that looks like owner/repo) is the remote.
    let cwd = s(envelope, "cwd")
        .or_else(|| s(&payload, "cwd"))
        .unwrap_or("/");
    let project_root = s(envelope, "project_root").or_else(|| s(&payload, "project_root"));
    let root = match project_root {
        Some(p)
            if p.starts_with('/') || p.get(1..3) == Some(":/") || p.get(1..3) == Some(":\\") =>
        {
            p
        }
        _ => cwd,
    };
    let remote = s(envelope, "repo_identifier").or(match project_root {
        Some(p) if !p.starts_with('/') && p.contains('/') => Some(p),
        _ => None,
    });
    let mut project = ProjectRef::derive(root, remote, &device_id);
    // VibeMon strips the host from every remote (`owner/repo`), so the
    // identifier cannot be normalised to `host/owner/repo` and the project
    // id falls back to the root-and-device form. Keep the identifier as the
    // display name so the two paths at least read the same; ids converge
    // once the install moves to `attempt hook`.
    if project.repo_remote.is_none()
        && let Some(id) = remote
    {
        project.name = id.to_string();
    }

    let session = s(envelope, "session_id")
        .or_else(|| s(&payload, "session_id"))
        .unwrap_or("unknown");

    let kind = match event_name {
        "activity" | "bash" => EventKind::ToolCallFinished,
        "tool_failure" => EventKind::ToolCallFailed,
        "prompt" => EventKind::PromptSubmitted,
        "stop" => EventKind::TurnStopped,
        "permission" => EventKind::PermissionRequested,
        "session_start" => EventKind::SessionStarted,
        "session_end" => EventKind::SessionEnded,
        "test" => EventKind::CaptureTest,
        _ => EventKind::Unknown,
    };

    let mut ev = Event::new(
        device_id,
        provider,
        event_name,
        kind,
        project,
        session,
        capture_mode,
        format!("vibemon-envelope-v2/{ADAPTER_VERSION}"),
    );
    ev.hook_version = Some(format!("vibemon-envelope-v{version}"));
    if let Some(ts) = s(envelope, "timestamp").and_then(Timestamp::parse) {
        ev.observed_at = ts;
    }
    if let Some(branch) = s(envelope, "branch") {
        ev.project.branch = Some(branch.to_string());
    }
    if let Some(head) = s(envelope, "head_sha") {
        ev.project.head = Some(head.to_string());
    }
    ev.attrs.insert("hook_event_name".into(), json!(event_name));
    ev.attrs.insert(
        "cwd".into(),
        json!(elide_home(&PortablePath::from_raw(cwd, Some(root)).logical)),
    );
    if let Some(h) = n(envelope, "local_hour") {
        ev.attrs.insert("x_vibemon_local_hour".into(), json!(h));
    }
    if let Some(d) = n(envelope, "local_dow") {
        ev.attrs.insert("x_vibemon_local_dow".into(), json!(d));
    }

    // Tool.
    let tool_name = s(&payload, "tool_name")
        .map(str::to_string)
        .or_else(|| s(&signals, "tool.name").map(str::to_string))
        .or_else(|| (event_name == "bash").then(|| "Bash".to_string()));
    if let Some(name) = tool_name {
        ev.tool = Some(ToolRef {
            name: name.clone(),
            category: classify_tool(&name),
            call_id: None,
        });
    }
    if let Some(ms) = n(&payload, "duration_ms").or_else(|| n(&signals, "tool.duration_ms")) {
        ev.duration_ms = Some(ms);
    }

    // Paths and file facts.
    if let Some(path) = payload.get("tool_input").and_then(|i| s(i, "file_path")) {
        let p = PortablePath::from_raw(path, Some(root));
        let facts = file_facts(&p);
        if let Some(ext) = facts.ext {
            ev.attrs.insert("file_ext".into(), json!(ext));
        }
        ev.attrs.insert("file_is_test".into(), json!(facts.is_test));
        ev.attrs
            .insert("file_is_config".into(), json!(facts.is_config));
        ev.attrs.insert("file_is_doc".into(), json!(facts.is_doc));
        ev.paths.push(p);
    } else if let Some(ext) = s(&signals, "file.ext") {
        ev.attrs.insert("file_ext".into(), json!(ext));
        for (sig, attr) in [
            ("file.is_test", "file_is_test"),
            ("file.is_config", "file_is_config"),
            ("file.is_doc", "file_is_doc"),
        ] {
            if let Some(v) = b(&signals, sig) {
                ev.attrs.insert(attr.into(), json!(v));
            }
        }
    }
    if let Some(a) = n(&signals, "lines.added") {
        ev.attrs.insert("lines_added".into(), json!(a));
    }
    if let Some(r) = n(&signals, "lines.removed") {
        ev.attrs.insert("lines_removed".into(), json!(r));
    }

    // Shell.
    if let Some(fine) = s(&signals, "bash.category") {
        ev.attrs
            .insert("command_category".into(), json!(coarse_of(fine)));
        ev.attrs.insert("command_subcategory".into(), json!(fine));
    }
    if let Some(bytes) = n(&signals, "bash.byte_len") {
        ev.attrs.insert("command_bytes".into(), json!(bytes));
    }

    // Prompt shape.
    for (sig, attr) in [
        ("prompt.chars", "prompt_chars"),
        ("prompt.line_count", "prompt_lines"),
    ] {
        if let Some(v) = n(&signals, sig) {
            ev.attrs.insert(attr.into(), json!(v));
        }
    }
    for (sig, attr) in [
        ("prompt.has_question", "prompt_has_question"),
        ("prompt.has_code_fence", "prompt_has_code_fence"),
    ] {
        if let Some(v) = b(&signals, sig) {
            ev.attrs.insert(attr.into(), json!(v));
        }
    }

    // Outcome.
    ev.outcome = Some(match kind {
        EventKind::ToolCallFailed => {
            let class = s(&signals, "failure.kind").unwrap_or("unknown");
            ev.attrs.insert("error_class".into(), json!(class));
            if let Some(bytes) = n(&signals, "failure.byte_len") {
                ev.attrs.insert("error_bytes".into(), json!(bytes));
            }
            Outcome::failure(Some(class.to_string()))
        }
        EventKind::ToolCallFinished | EventKind::PromptSubmitted | EventKind::TurnStopped => {
            Outcome::success()
        }
        _ => Outcome {
            status: attemptdb_core::event::OutcomeStatus::Unknown,
            class: None,
            exit_code: None,
        },
    });
    if matches!(
        kind,
        EventKind::SessionStarted
            | EventKind::SessionEnded
            | EventKind::CaptureTest
            | EventKind::PermissionRequested
            | EventKind::Unknown
    ) {
        ev.outcome = None;
    }

    // Session metadata.
    if let Some(model) = s(&payload, "model") {
        ev.agent.model = Some(model.to_string());
    }
    if let Some(source) = s(&payload, "source") {
        ev.attrs.insert("source".into(), json!(source));
    }
    if let Some(reason) = s(&payload, "reason") {
        ev.attrs.insert("reason".into(), json!(reason));
    }

    // Everything else in `signals` is a content-free scalar by contract;
    // keep it under the provider object so nothing is lost.
    if let Some(map) = signals.as_object() {
        let mut extra = Map::new();
        for (k, v) in map {
            let known = k.starts_with("file.")
                || k.starts_with("lines.")
                || k.starts_with("bash.")
                || k.starts_with("prompt.")
                || k.starts_with("failure.")
                || k == "tool.name"
                || k == "tool.duration_ms"
                || k == "commit.message";
            if known {
                continue;
            }
            match v {
                Value::String(x) if x.len() <= 64 => {
                    extra.insert(k.replace('.', "_"), v.clone());
                }
                Value::Number(_) | Value::Bool(_) => {
                    extra.insert(k.replace('.', "_"), v.clone());
                }
                _ => {}
            }
        }
        if !extra.is_empty() {
            ev.attrs.insert("provider".into(), Value::Object(extra));
        }
    }

    // The single content-bearing signal.
    if let Some(msg) = s(&signals, "commit.message") {
        ev.content = Some(EventContent {
            message: Some(msg.to_string()),
            ..Default::default()
        });
    }
    ev.raw = Some(envelope.clone());
    ev.apply_capture_mode();
    Ok(ev)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_spelling() {
        assert_eq!(provider_of("codex_cli"), Provider::Codex);
        assert_eq!(provider_of("gemini_cli"), Provider::GeminiCli);
        assert_eq!(provider_of("acme"), Provider::Other("acme".into()));
    }

    #[test]
    fn coarse_mapping() {
        assert_eq!(coarse_of("git.commit"), "git");
        assert_eq!(coarse_of("pkg.test"), "test");
        assert_eq!(coarse_of("net.request"), "network");
        assert_eq!(coarse_of("infra.docker"), "other");
    }

    #[test]
    fn rejects_other_versions_and_non_objects() {
        let d = DeviceId::derive(&["t", "d"]);
        assert!(normalise_envelope(d, CaptureMode::MetadataOnly, &json!([])).is_err());
        assert!(
            normalise_envelope(
                d,
                CaptureMode::MetadataOnly,
                &json!({"v": 3, "event": "bash", "agent": "claude_code"})
            )
            .is_err()
        );
        assert!(
            normalise_envelope(
                d,
                CaptureMode::MetadataOnly,
                &json!({"v": 2, "agent": "claude_code"})
            )
            .is_err()
        );
    }
}
