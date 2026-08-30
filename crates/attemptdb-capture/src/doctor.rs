//! `attempt doctor` / `attempt hook status`: report the real state of the
//! hook integration for every agent, separating *configured*, *trusted*,
//! *active*, *stale* and *unverified*.
//!
//! Formatting is the CLI's job; this module only produces data.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::agents::{AgentKind, DetectOptions, DetectedAgent, detect_agents_with, find_on_path};
use crate::install::{
    Scope, config_path_for, events_for, hook_command_binary, is_attempt_hook_object,
    preferred_hook_binary,
};
use crate::platform::{AppPaths, BINARY_NAME, app_paths, canonical_display_path, current_exe_path};

/// State of our hooks for one agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HookState {
    /// No AttemptDB entries in the config (or no config at all).
    NotInstalled,
    /// Entries present and current; activity information was not supplied.
    Configured,
    /// Entries point at a missing/old binary path, or the event set is old.
    Stale,
    /// Codex only: configured, but Codex has no (matching) trust record, so it
    /// will not run the hooks until the user approves them via `/hooks`.
    Untrusted,
    /// Configured and current, but no capture event has ever been observed.
    Unverified,
    /// Configured and a capture-test event went through the pipeline, but no
    /// real agent event has been observed yet.
    Verified,
    /// Configured, current, and at least one event was observed.
    Active,
}

/// What the caller knows about captured events for an agent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ActivitySummary {
    /// Timestamp of the most recent event (RFC 3339 or whatever the caller
    /// uses; this module only passes it through).
    pub last_event_at: Option<String>,
    pub event_count: u64,
    /// A capture-test event produced by `attempt hook install` was stored.
    #[serde(default)]
    pub capture_test_seen: bool,
}

/// Diagnosis for one agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AgentDiagnosis {
    pub agent: AgentKind,
    pub detected: bool,
    pub version: Option<String>,
    pub config_path: PathBuf,
    pub config_exists: bool,
    pub state: HookState,
    /// Events that carry one of our entries.
    pub events_configured: Vec<String>,
    /// Events we install that carry none of our entries.
    pub events_missing: Vec<String>,
    /// Events that carry one of our entries but are not in the current set.
    pub events_extra: Vec<String>,
    /// Binary path referenced by our entries (first one when they disagree).
    pub binary_path_in_config: Option<String>,
    /// Codex only: per-entry trust evaluation.
    pub trust: Option<Vec<codex_trust::EntryTrust>>,
    pub activity: Option<ActivitySummary>,
    pub notes: Vec<String>,
}

/// Whole-machine diagnosis.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Diagnosis {
    /// The binary hooks are expected to point at.
    pub binary: PathBuf,
    /// Whether an `attempt` binary is on `PATH` (not necessarily this one).
    pub binary_on_path: bool,
    pub paths: AppPaths,
    pub agents: Vec<AgentDiagnosis>,
}

/// Diagnose user-scope hooks for every supported agent. `activity` supplies
/// capture statistics per agent (`None` = unknown).
pub fn diagnose(activity: &dyn Fn(AgentKind) -> Option<ActivitySummary>) -> Diagnosis {
    diagnose_scope(&Scope::User, None, activity)
}

/// Diagnose hooks under an explicit scope and expected binary.
pub fn diagnose_scope(
    scope: &Scope,
    binary: Option<&Path>,
    activity: &dyn Fn(AgentKind) -> Option<ActivitySummary>,
) -> Diagnosis {
    let binary = binary
        .map(canonical_display_path)
        .unwrap_or_else(|| preferred_hook_binary(current_exe_path()));
    let detected = detect_agents_with(&DetectOptions::default());
    let agents = AgentKind::ALL
        .iter()
        .map(|&kind| {
            let det = detected.iter().find(|d| d.kind == kind);
            let config_path = config_path_for(kind, scope, det);
            let codex_toml = (kind == AgentKind::Codex)
                .then(|| kind.agent_dir().map(|d| d.join("config.toml")))
                .flatten();
            diagnose_agent(
                kind,
                det,
                config_path,
                &binary,
                codex_toml.as_deref(),
                activity(kind),
            )
        })
        .collect();
    Diagnosis {
        binary,
        binary_on_path: find_on_path(BINARY_NAME).is_some(),
        paths: app_paths(),
        agents,
    }
}

/// One of our entries as found in a config file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FoundEntry {
    pub event: String,
    /// Index of the matcher group within the event array (Cursor: index of
    /// the hook object).
    pub group_index: usize,
    /// Index of the hook object within the group (Cursor: always 0).
    pub handler_index: usize,
    pub matcher: Option<String>,
    pub command: String,
    pub timeout: Option<u64>,
}

/// Locate every AttemptDB entry in a parsed config.
pub fn find_our_entries(kind: AgentKind, config: &Value) -> Vec<FoundEntry> {
    let mut out = Vec::new();
    let Some(hooks) = config.get("hooks").and_then(Value::as_object) else {
        return out;
    };
    for (event, arr) in hooks {
        let Some(entries) = arr.as_array() else {
            continue;
        };
        for (gi, entry) in entries.iter().enumerate() {
            if kind == AgentKind::Cursor {
                if is_attempt_hook_object(entry) {
                    out.push(found(event, gi, 0, None, entry));
                }
                continue;
            }
            let matcher = entry
                .get("matcher")
                .and_then(Value::as_str)
                .map(str::to_string);
            let Some(inner) = entry.get("hooks").and_then(Value::as_array) else {
                continue;
            };
            for (hi, hook) in inner.iter().enumerate() {
                if is_attempt_hook_object(hook) {
                    out.push(found(event, gi, hi, matcher.clone(), hook));
                }
            }
        }
    }
    out
}

fn found(event: &str, gi: usize, hi: usize, matcher: Option<String>, hook: &Value) -> FoundEntry {
    FoundEntry {
        event: event.to_string(),
        group_index: gi,
        handler_index: hi,
        matcher,
        command: hook
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        timeout: hook.get("timeout").and_then(Value::as_u64),
    }
}

/// Diagnose one agent from explicit inputs (testable without a home dir).
///
/// `codex_config_toml` is only consulted for [`AgentKind::Codex`].
pub fn diagnose_agent(
    kind: AgentKind,
    detected: Option<&DetectedAgent>,
    config_path: Option<PathBuf>,
    expected_binary: &Path,
    codex_config_toml: Option<&Path>,
    activity: Option<ActivitySummary>,
) -> AgentDiagnosis {
    let mut d = AgentDiagnosis {
        agent: kind,
        detected: detected.is_some(),
        version: detected.and_then(|d| d.version.clone()),
        config_path: config_path.clone().unwrap_or_default(),
        config_exists: false,
        state: HookState::NotInstalled,
        events_configured: Vec::new(),
        events_missing: events_for(kind).iter().map(|e| e.to_string()).collect(),
        events_extra: Vec::new(),
        binary_path_in_config: None,
        trust: None,
        activity: activity.clone(),
        notes: Vec::new(),
    };
    if detected.is_none() {
        d.notes.push(format!(
            "{} not detected (no {} directory and no `{}` on PATH)",
            kind.display_name(),
            kind.dir_name(),
            kind.binary_name()
        ));
    }
    let Some(path) = config_path else {
        d.notes
            .push("this scope has no config file for this agent".into());
        return d;
    };
    d.config_exists = path.is_file();
    if !d.config_exists {
        return d;
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => {
            d.notes.push(format!("cannot read {}: {e}", path.display()));
            return d;
        }
    };
    let config: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            d.notes
                .push(format!("{} is not valid JSON: {e}", path.display()));
            return d;
        }
    };

    let entries = find_our_entries(kind, &config);
    let wanted: Vec<&str> = events_for(kind).to_vec();
    let mut per_event: BTreeMap<&str, usize> = BTreeMap::new();
    for e in &entries {
        *per_event.entry(e.event.as_str()).or_default() += 1;
    }
    d.events_configured = wanted
        .iter()
        .filter(|e| per_event.contains_key(**e))
        .map(|e| e.to_string())
        .chain(
            per_event
                .keys()
                .filter(|e| !wanted.contains(e))
                .map(|e| e.to_string()),
        )
        .collect();
    d.events_missing = wanted
        .iter()
        .filter(|e| !per_event.contains_key(**e))
        .map(|e| e.to_string())
        .collect();
    d.events_extra = per_event
        .keys()
        .filter(|e| !wanted.contains(e))
        .map(|e| e.to_string())
        .collect();
    if entries.is_empty() {
        d.state = HookState::NotInstalled;
        return d;
    }

    let mut stale = false;
    let binaries: BTreeSet<String> = entries
        .iter()
        .filter_map(|e| hook_command_binary(&e.command))
        .collect();
    d.binary_path_in_config = binaries.iter().next().cloned();
    if binaries.len() > 1 {
        d.notes.push(format!(
            "entries reference {} different binary paths",
            binaries.len()
        ));
        stale = true;
    }
    for b in &binaries {
        let p = Path::new(b);
        if !p.exists() {
            d.notes
                .push(format!("configured binary does not exist: {b}"));
            stale = true;
        } else if !same_file(p, expected_binary) {
            d.notes.push(format!(
                "configured binary {b} differs from the expected binary {}",
                expected_binary.display()
            ));
            stale = true;
        }
    }
    if !d.events_missing.is_empty() {
        d.notes
            .push(format!("missing events: {}", d.events_missing.join(", ")));
        stale = true;
    }
    if !d.events_extra.is_empty() {
        d.notes
            .push(format!("obsolete events: {}", d.events_extra.join(", ")));
        stale = true;
    }
    let dupes: Vec<&str> = per_event
        .iter()
        .filter(|(_, n)| **n > 1)
        .map(|(e, _)| *e)
        .collect();
    if !dupes.is_empty() {
        d.notes
            .push(format!("duplicate entries for: {}", dupes.join(", ")));
        stale = true;
    }

    let mut untrusted = false;
    if kind == AgentKind::Codex {
        match codex_config_toml.map(std::fs::read_to_string) {
            Some(Ok(toml_text)) => match codex_trust::read_hook_states(&toml_text) {
                Ok(states) => {
                    let evaluated = codex_trust::evaluate(&path, &entries, &states);
                    for t in &evaluated {
                        match t.status {
                            codex_trust::TrustStatus::Trusted => {}
                            codex_trust::TrustStatus::Modified => {
                                d.notes.push(format!("{}: trusted hash does not match the current entry (re-approve in /hooks)", t.event));
                                untrusted = true;
                            }
                            codex_trust::TrustStatus::Untrusted => {
                                d.notes.push(format!(
                                    "{}: not yet trusted (approve in /hooks)",
                                    t.event
                                ));
                                untrusted = true;
                            }
                        }
                        if !t.enabled {
                            d.notes
                                .push(format!("{}: disabled in Codex /hooks", t.event));
                        }
                    }
                    d.trust = Some(evaluated);
                }
                Err(e) => {
                    d.notes.push(format!(
                        "cannot parse Codex config.toml: {e}; trust state unknown"
                    ));
                    untrusted = true;
                }
            },
            Some(Err(_)) | None => {
                d.notes.push(
                    "Codex config.toml not found; hooks are not trusted yet (approve in /hooks)"
                        .into(),
                );
                untrusted = true;
            }
        }
    }

    d.state = if stale {
        HookState::Stale
    } else if untrusted {
        HookState::Untrusted
    } else {
        match &activity {
            None => HookState::Configured,
            Some(a) if a.event_count > 0 => HookState::Active,
            Some(a) if a.capture_test_seen => HookState::Verified,
            Some(_) => HookState::Unverified,
        }
    };
    d
}

/// Compare two paths after canonicalisation (case-insensitively on Windows).
fn same_file(a: &Path, b: &Path) -> bool {
    let ca = canonical_display_path(a);
    let cb = canonical_display_path(b);
    if cfg!(windows) {
        ca.to_string_lossy()
            .eq_ignore_ascii_case(&cb.to_string_lossy())
    } else {
        ca == cb
    }
}

/// Codex hook trust.
///
/// Codex refuses to run a hook until the user approves it via `/hooks`; the
/// approval is recorded in `config.toml` as
///
/// ```toml
/// [hooks.state."<abs hooks.json path>:<event_snake>:<group>:<handler>"]
/// trusted_hash = "sha256:<hex>"
/// ```
///
/// We never write that table. The hash is reproduced here exactly as Codex
/// computes it (`codex-rs/hooks/src/engine/discovery.rs::hook_hash` +
/// `codex-rs/config/src/fingerprint.rs::version_for_toml`): the normalised
/// identity `{event_name, matcher?, hooks: [{type, command, timeout, async}]}`
/// is serialised to canonical (key-sorted, compact) JSON and SHA-256 hashed.
/// The scheme was verified against real `trusted_hash` values, so a
/// [`TrustStatus::Trusted`] verdict means Codex will really run the hook.
pub mod codex_trust {
    use super::*;
    use sha2::{Digest, Sha256};

    /// One `[hooks.state."..."]` record.
    #[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
    pub struct HookStateEntry {
        pub enabled: Option<bool>,
        pub trusted_hash: Option<String>,
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum TrustStatus {
        /// A trust record exists and its hash matches the entry.
        Trusted,
        /// A trust record exists but the entry changed since approval.
        Modified,
        /// No trust record.
        Untrusted,
    }

    /// Trust evaluation for one of our entries.
    #[derive(Clone, Debug, PartialEq, Eq, Serialize)]
    pub struct EntryTrust {
        pub event: String,
        pub key: String,
        pub expected_hash: String,
        pub status: TrustStatus,
        /// `false` when the user disabled the hook in `/hooks`.
        pub enabled: bool,
    }

    /// Parse `[hooks.state.*]` from a Codex `config.toml`.
    pub fn read_hook_states(
        toml_text: &str,
    ) -> Result<HashMap<String, HookStateEntry>, toml_edit::TomlError> {
        let doc: toml_edit::DocumentMut = toml_text.parse()?;
        let mut out = HashMap::new();
        let Some(state) = doc
            .get("hooks")
            .and_then(|h| h.as_table_like())
            .and_then(|h| h.get("state"))
            .and_then(|s| s.as_table_like())
        else {
            return Ok(out);
        };
        for (key, item) in state.iter() {
            let Some(t) = item.as_table_like() else {
                continue;
            };
            out.insert(
                key.to_string(),
                HookStateEntry {
                    enabled: t.get("enabled").and_then(|v| v.as_bool()),
                    trusted_hash: t
                        .get("trusted_hash")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                },
            );
        }
        Ok(out)
    }

    /// `PostToolUse` -> `post_tool_use` (Codex's `hook_event_key_label`).
    pub fn event_snake(event: &str) -> String {
        let mut out = String::with_capacity(event.len() + 4);
        for (i, c) in event.chars().enumerate() {
            if c.is_ascii_uppercase() {
                if i > 0 {
                    out.push('_');
                }
                out.push(c.to_ascii_lowercase());
            } else {
                out.push(c);
            }
        }
        out
    }

    /// `<hooks.json path>:<event_snake>:<group>:<handler>`.
    pub fn hook_key(
        hooks_json_path: &str,
        event: &str,
        group_index: usize,
        handler_index: usize,
    ) -> String {
        format!(
            "{hooks_json_path}:{}:{group_index}:{handler_index}",
            event_snake(event)
        )
    }

    /// The hash Codex records when the user trusts a command hook.
    pub fn hook_hash(
        event: &str,
        matcher: Option<&str>,
        command: &str,
        timeout: Option<u64>,
    ) -> String {
        let snake = event_snake(event);
        let timeout = match snake.as_str() {
            "session_end" | "interrupt" => timeout.unwrap_or(1).clamp(1, 3),
            _ => timeout.unwrap_or(600).max(1),
        };
        let matcher = match snake.as_str() {
            "user_prompt_submit" | "stop" | "interrupt" => None,
            _ => matcher,
        };
        let mut identity = serde_json::json!({
            "event_name": snake,
            "hooks": [{ "type": "command", "command": command, "timeout": timeout, "async": false }],
        });
        if let Some(m) = matcher {
            identity["matcher"] = Value::String(m.to_string());
        }
        let bytes = serde_json::to_vec(&canonical_json(&identity)).unwrap_or_default();
        let digest = Sha256::digest(&bytes);
        let mut hex = String::with_capacity(64);
        for b in digest {
            use std::fmt::Write;
            let _ = write!(hex, "{b:02x}");
        }
        format!("sha256:{hex}")
    }

    fn canonical_json(v: &Value) -> Value {
        match v {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                let mut out = serde_json::Map::new();
                for k in keys {
                    out.insert(k.clone(), canonical_json(&map[k]));
                }
                Value::Object(out)
            }
            Value::Array(items) => Value::Array(items.iter().map(canonical_json).collect()),
            other => other.clone(),
        }
    }

    /// Evaluate trust for our entries in `hooks_json_path`.
    pub fn evaluate(
        hooks_json_path: &Path,
        entries: &[FoundEntry],
        states: &HashMap<String, HookStateEntry>,
    ) -> Vec<EntryTrust> {
        let key_source = hooks_json_path.to_string_lossy().into_owned();
        let canonical_source = canonical_display_path(hooks_json_path);
        entries
            .iter()
            .map(|e| {
                let key = hook_key(&key_source, &e.event, e.group_index, e.handler_index);
                let state = states.get(&key).or_else(|| {
                    // Same file spelled differently (symlinked home, etc.).
                    let suffix = format!(
                        ":{}:{}:{}",
                        event_snake(&e.event),
                        e.group_index,
                        e.handler_index
                    );
                    states.iter().find_map(|(k, v)| {
                        let path_part = k.strip_suffix(&suffix)?;
                        (canonical_display_path(Path::new(path_part)) == canonical_source)
                            .then_some(v)
                    })
                });
                let expected_hash =
                    hook_hash(&e.event, e.matcher.as_deref(), &e.command, e.timeout);
                let status = match state.and_then(|s| s.trusted_hash.as_deref()) {
                    Some(h) if h == expected_hash => TrustStatus::Trusted,
                    Some(_) => TrustStatus::Modified,
                    None => TrustStatus::Untrusted,
                };
                EntryTrust {
                    event: e.event.clone(),
                    key,
                    expected_hash,
                    status,
                    enabled: state.and_then(|s| s.enabled) != Some(false),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::codex_trust::*;
    use super::*;
    use crate::install::{install_to, planned_config};
    use serde_json::json;
    use std::fs;

    #[test]
    fn codex_hash_matches_real_trust_records() {
        // Vectors observed in a real ~/.codex/config.toml (Codex 0.150).
        assert_eq!(
            hook_hash(
                "PostToolUse",
                Some("Edit|Write|apply_patch"),
                "bash ~/.vibemon/notify.sh activity codex_cli",
                Some(10)
            ),
            "sha256:306ce275f2cc817572187285d692d4878cca2fb0b823d32fea8b5b28f2595fd0"
        );
        // SessionEnd clamps the timeout to 3 s before hashing.
        assert_eq!(
            hook_hash(
                "SessionEnd",
                None,
                "bash ~/.vibemon/notify.sh session_end codex_cli",
                Some(10)
            ),
            "sha256:59015a3376fd5e5d8ffb5bf1adf4be4ebc610622c674a7546a32a06ac4a47a61"
        );
        // Stop ignores matchers entirely.
        assert_eq!(
            hook_hash(
                "Stop",
                Some("anything"),
                "bash ~/.vibemon/notify.sh stop codex_cli",
                Some(10)
            ),
            "sha256:c81392377e7e0c36bc8d845d39bdf7f3e61c96c5862df1bcd88515de7839af43"
        );
        assert_eq!(event_snake("UserPromptSubmit"), "user_prompt_submit");
        assert_eq!(
            hook_key("/h/.codex/hooks.json", "PreToolUse", 2, 1),
            "/h/.codex/hooks.json:pre_tool_use:2:1"
        );
    }

    #[test]
    fn reads_hook_state_table() {
        let toml = r#"
model = "gpt-5"

[hooks.state]

[hooks.state."/h/.codex/hooks.json:stop:0:0"]
trusted_hash = "sha256:abc"

[hooks.state."/h/.codex/hooks.json:session_start:1:0"]
enabled = false
trusted_hash = "sha256:def"
"#;
        let states = read_hook_states(toml).unwrap();
        assert_eq!(states.len(), 2);
        assert_eq!(
            states["/h/.codex/hooks.json:stop:0:0"]
                .trusted_hash
                .as_deref(),
            Some("sha256:abc")
        );
        assert_eq!(
            states["/h/.codex/hooks.json:session_start:1:0"].enabled,
            Some(false)
        );
        assert!(read_hook_states("model = \"x\"\n").unwrap().is_empty());
        assert!(read_hook_states("= broken").is_err());
    }

    #[test]
    fn evaluates_trust_per_entry() {
        let cmd = "'/opt/attemptdb/attempt' hook codex";
        let config = planned_config(AgentKind::Codex, cmd);
        let path = Path::new("/h/.codex/hooks.json");
        let entries = find_our_entries(AgentKind::Codex, &config);
        assert_eq!(entries.len(), crate::install::CODEX_EVENTS.len());

        let mut states = HashMap::new();
        for e in &entries {
            let key = hook_key(
                "/h/.codex/hooks.json",
                &e.event,
                e.group_index,
                e.handler_index,
            );
            let hash = if e.event == "Stop" {
                "sha256:stale".to_string()
            } else {
                hook_hash(&e.event, None, cmd, e.timeout)
            };
            if e.event != "SessionEnd" {
                states.insert(
                    key,
                    HookStateEntry {
                        enabled: Some(e.event != "PreToolUse"),
                        trusted_hash: Some(hash),
                    },
                );
            }
        }
        let trust = evaluate(path, &entries, &states);
        let by_event: HashMap<&str, &EntryTrust> =
            trust.iter().map(|t| (t.event.as_str(), t)).collect();
        assert_eq!(by_event["SessionStart"].status, TrustStatus::Trusted);
        assert_eq!(by_event["SessionEnd"].status, TrustStatus::Untrusted);
        assert_eq!(by_event["Stop"].status, TrustStatus::Modified);
        assert!(!by_event["PreToolUse"].enabled);
        assert!(by_event["SessionStart"].enabled);
    }

    fn fake_binary(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, "#!/bin/sh\n").unwrap();
        p
    }

    #[test]
    fn states_for_claude_config() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = fake_binary(tmp.path(), "attempt");
        let cfg = tmp.path().join("settings.json");
        let cmd = format!("'{}' hook claude-code", bin.display());

        let d = diagnose_agent(
            AgentKind::ClaudeCode,
            None,
            Some(cfg.clone()),
            &bin,
            None,
            None,
        );
        assert_eq!(d.state, HookState::NotInstalled);
        assert!(!d.config_exists);

        install_to(AgentKind::ClaudeCode, &cfg, &cmd, false).unwrap();
        let d = diagnose_agent(
            AgentKind::ClaudeCode,
            None,
            Some(cfg.clone()),
            &bin,
            None,
            None,
        );
        assert_eq!(d.state, HookState::Configured, "{:?}", d.notes);
        assert!(d.events_missing.is_empty());
        assert_eq!(
            d.binary_path_in_config.as_deref(),
            Some(bin.to_string_lossy().as_ref())
        );

        let unverified = diagnose_agent(
            AgentKind::ClaudeCode,
            None,
            Some(cfg.clone()),
            &bin,
            None,
            Some(ActivitySummary::default()),
        );
        assert_eq!(unverified.state, HookState::Unverified);
        let active = diagnose_agent(
            AgentKind::ClaudeCode,
            None,
            Some(cfg.clone()),
            &bin,
            None,
            Some(ActivitySummary {
                last_event_at: Some("2026-08-28T00:00:00Z".into()),
                event_count: 3,
                capture_test_seen: false,
            }),
        );
        assert_eq!(active.state, HookState::Active);

        // Different (existing) binary -> stale.
        let other = fake_binary(tmp.path(), "attempt-old");
        let stale = diagnose_agent(
            AgentKind::ClaudeCode,
            None,
            Some(cfg.clone()),
            &other,
            None,
            Some(ActivitySummary {
                last_event_at: None,
                event_count: 9,
                capture_test_seen: false,
            }),
        );
        assert_eq!(stale.state, HookState::Stale);

        // Missing binary -> stale.
        fs::remove_file(&bin).unwrap();
        let stale = diagnose_agent(
            AgentKind::ClaudeCode,
            None,
            Some(cfg.clone()),
            &bin,
            None,
            None,
        );
        assert_eq!(stale.state, HookState::Stale);
        assert!(stale.notes.iter().any(|n| n.contains("does not exist")));
    }

    #[test]
    fn stale_when_event_set_is_old() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = fake_binary(tmp.path(), "attempt");
        let cfg = tmp.path().join("hooks.json");
        let cmd = format!("'{}' hook cursor", bin.display());
        let mut v = planned_config(AgentKind::Cursor, &cmd);
        v["hooks"].as_object_mut().unwrap().shift_remove("stop");
        v["hooks"]["afterFileCreate"] = json!([{ "command": cmd, "timeout": 5000 }]);
        fs::write(&cfg, serde_json::to_string_pretty(&v).unwrap()).unwrap();
        let d = diagnose_agent(AgentKind::Cursor, None, Some(cfg), &bin, None, None);
        assert_eq!(d.state, HookState::Stale);
        assert_eq!(d.events_missing, vec!["stop"]);
        assert_eq!(d.events_extra, vec!["afterFileCreate"]);
    }

    #[test]
    fn codex_untrusted_until_hashes_match() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = fake_binary(tmp.path(), "attempt");
        let codex_home = tmp.path().join(".codex");
        let cfg = codex_home.join("hooks.json");
        let toml_path = codex_home.join("config.toml");
        let cmd = format!("'{}' hook codex", bin.display());
        install_to(AgentKind::Codex, &cfg, &cmd, false).unwrap();

        let d = diagnose_agent(
            AgentKind::Codex,
            None,
            Some(cfg.clone()),
            &bin,
            Some(&toml_path),
            None,
        );
        assert_eq!(d.state, HookState::Untrusted);

        fs::write(&toml_path, "model = \"x\"\n").unwrap();
        let d = diagnose_agent(
            AgentKind::Codex,
            None,
            Some(cfg.clone()),
            &bin,
            Some(&toml_path),
            None,
        );
        assert_eq!(d.state, HookState::Untrusted);
        assert!(
            d.trust
                .as_ref()
                .unwrap()
                .iter()
                .all(|t| t.status == TrustStatus::Untrusted)
        );

        // Simulate the user trusting everything in /hooks.
        let config: Value = serde_json::from_str(&fs::read_to_string(&cfg).unwrap()).unwrap();
        let mut toml = String::from("model = \"x\"\n\n[hooks.state]\n");
        for e in find_our_entries(AgentKind::Codex, &config) {
            let key = hook_key(
                &cfg.to_string_lossy(),
                &e.event,
                e.group_index,
                e.handler_index,
            );
            let hash = hook_hash(&e.event, e.matcher.as_deref(), &e.command, e.timeout);
            // The key is a filesystem path, so on Windows it carries
            // backslashes. A TOML basic string treats those as escapes, which
            // silently produces a different key and a spurious Untrusted.
            let key = key.replace('\\', "\\\\").replace('"', "\\\"");
            toml.push_str(&format!(
                "\n[hooks.state.\"{key}\"]\ntrusted_hash = \"{hash}\"\n"
            ));
        }
        fs::write(&toml_path, toml).unwrap();
        let d = diagnose_agent(
            AgentKind::Codex,
            None,
            Some(cfg.clone()),
            &bin,
            Some(&toml_path),
            Some(ActivitySummary {
                last_event_at: None,
                event_count: 1,
                capture_test_seen: false,
            }),
        );
        assert_eq!(d.state, HookState::Active, "{:?}", d.notes);
        assert!(
            d.trust
                .unwrap()
                .iter()
                .all(|t| t.status == TrustStatus::Trusted)
        );
    }

    #[test]
    fn whole_machine_diagnosis_does_not_panic() {
        let diag = diagnose(&|_| None);
        assert_eq!(diag.agents.len(), AgentKind::ALL.len());
    }
}
