//! `attempt doctor` / `attempt hook status`: report the real state of the
//! hook integration for every agent, separating *configured*, *trusted*,
//! *active*, *stale* and *unverified*.
//!
//! Formatting is the CLI's job; this module only produces data.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use attemptdb_core::{Event, EventKind, Timestamp};
use serde::Serialize;
use serde_json::Value;

use crate::agents::{
    AgentKind, DetectOptions, DetectedAgent, detect_agents_with, display_path, find_on_path,
};
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
    /// Config is outdated, or no real capture arrived within seven days.
    Stale,
    /// Codex only: configured, but Codex has no (matching) trust record, so it
    /// will not run the hooks until the user approves them via `/hooks`.
    Untrusted,
    /// The provider or at least one required subscription was disabled.
    Disabled,
    /// Configured and current, but no capture event has ever been observed.
    Unverified,
    /// Configured and a capture-test event went through the pipeline, but no
    /// real agent event has been observed yet.
    Verified,
    /// Configured, current, and a real capture occurred within seven days.
    Active,
}

/// What the caller knows about captured events for an agent.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ActivitySummary {
    /// Latest real hook capture time, RFC 3339 UTC (not reconstructed history).
    pub last_event_at: Option<String>,
    pub event_count: u64,
    /// A capture-test event produced by `attempt hook install` was stored.
    #[serde(default)]
    pub capture_test_seen: bool,
}

impl ActivitySummary {
    /// Imports and synthetic self-tests cannot prove that the provider runs
    /// hooks. Compare timestamps explicitly; segment scan order is not time.
    pub fn record(&mut self, event: &Event) {
        if event.kind == EventKind::CaptureTest {
            self.capture_test_seen = true;
        } else if event.attrs.get("reconstructed").and_then(Value::as_bool) != Some(true) {
            self.event_count += 1;
            let previous = self.last_event_at.as_deref().and_then(Timestamp::parse);
            if previous.is_none_or(|at| event.captured_at > at) {
                self.last_event_at = Some(event.captured_at.to_rfc3339());
            }
        }
    }
}

const ACTIVE_WINDOW_US: i64 = 7 * 24 * 60 * 60 * 1_000_000;

fn activity_state(activity: Option<&ActivitySummary>, now: Timestamp) -> HookState {
    match activity {
        None => HookState::Configured,
        Some(a) if a.event_count > 0 => match a.last_event_at.as_deref().and_then(Timestamp::parse)
        {
            Some(at) if now.as_micros().saturating_sub(at.as_micros()) <= ACTIVE_WINDOW_US => {
                HookState::Active
            }
            Some(_) => HookState::Stale,
            None => HookState::Unverified,
        },
        Some(a) if a.capture_test_seen => HookState::Verified,
        Some(_) => HookState::Unverified,
    }
}

/// Diagnosis for one agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AgentDiagnosis {
    pub agent: AgentKind,
    pub detected: bool,
    pub version: Option<String>,
    /// The agent home this entry is about. Claude Code is reported once per
    /// config directory (`~/.claude`, `~/.claude-work`, ...), each with its
    /// own state; `None` when the agent was not detected.
    #[serde(default)]
    pub config_dir: Option<PathBuf>,
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
    /// The entries on disk are out of date (events missing or obsolete, a
    /// binary path that moved or is gone, duplicates): `attempt setup`
    /// rewrites them. Not set for a `stale` verdict that only means no
    /// capture arrived for a week.
    #[serde(default)]
    pub config_stale: bool,
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
    diagnose_scope_with(scope, binary, &[], activity)
}

/// [`diagnose_scope`] for an explicit list of Claude Code config directories
/// (`--claude-config-dir`; empty = detect every one).
///
/// Claude Code appears once per config directory, each judged by its own
/// config file. Captured events are counted per provider, not per directory
/// (a hook payload does not say which account it came from), so the activity
/// that makes an entry *active* is attached to the first directory only; the
/// others report what their config says (`configured`, `stale`, ...) and say
/// so.
pub fn diagnose_scope_with(
    scope: &Scope,
    binary: Option<&Path>,
    claude_config_dirs: &[PathBuf],
    activity: &dyn Fn(AgentKind) -> Option<ActivitySummary>,
) -> Diagnosis {
    let binary = binary
        .map(canonical_display_path)
        .unwrap_or_else(|| preferred_hook_binary(current_exe_path()));
    let detected = detect_agents_with(&DetectOptions {
        claude_config_dirs: claude_config_dirs.to_vec(),
        ..DetectOptions::default()
    });
    let mut agents = Vec::new();
    for &kind in &AgentKind::ALL {
        let dets: Vec<&DetectedAgent> = detected.iter().filter(|d| d.kind == kind).collect();
        agents.extend(diagnose_kind(scope, &binary, kind, &dets, activity(kind)));
    }
    Diagnosis {
        binary,
        binary_on_path: find_on_path(BINARY_NAME).is_some(),
        paths: app_paths(),
        agents,
    }
}

/// Every diagnosis for one agent: one entry when it has one home, one per
/// config directory when it has several (Claude Code), one "not detected"
/// entry when it has none. Project and local scopes name a single file, so
/// they get a single entry whatever was detected.
fn diagnose_kind(
    scope: &Scope,
    binary: &Path,
    kind: AgentKind,
    detected: &[&DetectedAgent],
    activity: Option<ActivitySummary>,
) -> Vec<AgentDiagnosis> {
    let dets = if *scope == Scope::User {
        detected
    } else {
        &detected[..detected.len().min(1)]
    };
    if dets.is_empty() {
        return vec![diagnose_agent(
            kind,
            None,
            config_path_for(kind, scope, None),
            binary,
            None,
            activity,
        )];
    }
    let several = dets.len() > 1;
    let mut out = Vec::new();
    for (i, det) in dets.iter().enumerate() {
        let codex_toml = (kind == AgentKind::Codex)
            .then(|| kind.agent_dir().map(|d| d.join("config.toml")))
            .flatten();
        let mut d = diagnose_agent(
            kind,
            Some(det),
            config_path_for(kind, scope, Some(det)),
            binary,
            codex_toml.as_deref(),
            if i == 0 { activity.clone() } else { None },
        );
        if several && d.state == HookState::NotInstalled {
            d.notes.push(format!(
                "{} has no AttemptDB hooks: sessions run with this config directory are not captured (`attempt setup` wires every detected one)",
                display_path(&det.config_dir)
            ));
        }
        if i > 0 && d.state != HookState::NotInstalled {
            d.notes.push(
                "captured events are counted per provider, not per config directory: activity is shown on the first entry"
                    .into(),
            );
        }
        out.push(d);
    }
    out
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
        config_dir: detected.map(|d| d.config_dir.clone()),
        config_path: config_path.clone().unwrap_or_default(),
        config_exists: false,
        state: HookState::NotInstalled,
        events_configured: Vec::new(),
        events_missing: events_for(kind).iter().map(|e| e.to_string()).collect(),
        events_extra: Vec::new(),
        binary_path_in_config: None,
        trust: None,
        activity: activity.clone(),
        config_stale: false,
        notes: Vec::new(),
    };
    if detected.is_none() {
        d.notes.push(match kind.missing_home_from_env() {
            Some((var, dir)) => format!(
                "{} not detected: {var} is set to {}, which does not exist",
                kind.display_name(),
                dir.display()
            ),
            None => format!(
                "{} not detected (no {} directory and no `{}` on PATH)",
                kind.display_name(),
                kind.dir_name(),
                kind.binary_name()
            ),
        });
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
    let body = text.strip_prefix(crate::install::UTF8_BOM).unwrap_or(&text);
    let config: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(e) => {
            let has_comments =
                serde_json::from_str::<Value>(&crate::install::strip_json_comments(body)).is_ok();
            d.notes.push(if has_comments {
                format!(
                    "{} contains comments (// or /* */); `attempt setup` cannot edit it without losing them, so move the comments out of the file",
                    path.display()
                )
            } else {
                format!("{} is not valid JSON: {e}", path.display())
            });
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

    d.config_stale = stale;
    let mut disabled = kind == AgentKind::ClaudeCode
        && config.get("disableAllHooks").and_then(Value::as_bool) == Some(true);
    if disabled {
        d.notes.push("Claude Code disableAllHooks is true".into());
    }
    let mut untrusted = false;
    if kind == AgentKind::Codex {
        match codex_config_toml.map(std::fs::read_to_string) {
            Some(Ok(toml_text)) => match codex_trust::read_hook_states(&toml_text) {
                Ok(states) => {
                    if let Ok(doc) = toml_text.parse::<toml_edit::DocumentMut>() {
                        let feature = doc
                            .get("features")
                            .and_then(|f| f.get("hooks").or_else(|| f.get("codex_hooks")))
                            .and_then(|v| v.as_bool());
                        if feature == Some(false) {
                            disabled = true;
                            d.notes
                                .push("Codex hooks are disabled in [features]".into());
                        }
                    }
                    let evaluated = codex_trust::evaluate(&path, &entries, &states);
                    // One line per kind of problem, with the events it is
                    // about: twelve lines that differ only in the event name
                    // hide the one thing to do.
                    let of = |pick: &dyn Fn(&codex_trust::EntryTrust) -> bool| -> Vec<&str> {
                        evaluated
                            .iter()
                            .filter(|t| pick(t))
                            .map(|t| t.event.as_str())
                            .collect()
                    };
                    let modified = of(&|t| t.status == codex_trust::TrustStatus::Modified);
                    let unapproved = of(&|t| t.status == codex_trust::TrustStatus::Untrusted);
                    let switched_off = of(&|t| !t.enabled);
                    let total = evaluated.len();
                    if !modified.is_empty() {
                        d.notes.push(format!(
                            "{} hook entr{} changed since approval (trusted hash does not match; re-approve in /hooks): {}",
                            count_of(modified.len(), total),
                            if modified.len() == 1 { "y" } else { "ies" },
                            events_list(&modified)
                        ));
                        untrusted = true;
                    }
                    if !unapproved.is_empty() {
                        d.notes.push(format!(
                            "{} hook entr{} not yet trusted (approve in /hooks): {}",
                            count_of(unapproved.len(), total),
                            if unapproved.len() == 1 { "y" } else { "ies" },
                            events_list(&unapproved)
                        ));
                        untrusted = true;
                    }
                    if !switched_off.is_empty() {
                        disabled = true;
                        d.notes.push(format!(
                            "{} hook entr{} disabled in Codex /hooks: {}",
                            count_of(switched_off.len(), total),
                            if switched_off.len() == 1 { "y" } else { "ies" },
                            events_list(&switched_off)
                        ));
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

    d.state = if disabled {
        HookState::Disabled
    } else if stale {
        HookState::Stale
    } else if untrusted {
        HookState::Untrusted
    } else {
        let state = activity_state(activity.as_ref(), Timestamp::now());
        if state == HookState::Stale {
            d.notes
                .push("no real hook capture within the last seven days".into());
        }
        state
    };
    d
}

/// "all 12" or "3 of 12".
fn count_of(n: usize, total: usize) -> String {
    if n == total && total > 1 {
        format!("all {total}")
    } else {
        format!("{n} of {total}")
    }
}

/// The first few event names, then how many more.
fn events_list(events: &[&str]) -> String {
    const SHOWN: usize = 3;
    if events.len() <= SHOWN {
        events.join(", ")
    } else {
        format!(
            "{}, and {} more",
            events[..SHOWN].join(", "),
            events.len() - SHOWN
        )
    }
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

// ---------------------------------------------------------------------------
// The capture path itself: config, database choice, identity, encryption
// ---------------------------------------------------------------------------

/// What `attempt doctor` reports about the capture path beyond the agents'
/// hook entries: the things that change what a hook records without any
/// error reaching the agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CaptureHealth {
    /// Why `config.json` is not used as written. Capture is metadata-only
    /// meanwhile (`Config::load_or_default` fails closed).
    pub config_error: Option<String>,
    /// Project-local `.attemptdb/` directories found above the working
    /// directory and not used (symlinks, another owner), with the reason.
    pub ignored_local_databases: Vec<crate::locator::IgnoredLocalDb>,
    /// Corrupt `device.json` files that were moved aside; each marks a
    /// moment this machine got a new device id.
    pub device_repairs: Vec<PathBuf>,
    /// What the last writer decided about content encryption (see
    /// `crate::keys`), when it recorded anything.
    pub encryption: Option<crate::keys::EncryptionState>,
}

/// Gather [`CaptureHealth`] for the database `locator` points at.
pub fn capture_health(
    locator: &crate::locator::Locator,
    config: &crate::config::Config,
) -> CaptureHealth {
    CaptureHealth {
        config_error: config.load_error.clone(),
        ignored_local_databases: locator.ignored_local.clone(),
        device_repairs: crate::config::DeviceRecord::corrupt_backups(&locator.paths.data_dir),
        encryption: attemptdb_storage::Identity::load(&locator.db_dir)
            .ok()
            .and_then(|identity| crate::keys::read_state(locator, identity.db_id)),
    }
}

impl CaptureHealth {
    /// Something is actively costing data: the config is being ignored, or
    /// content is being dropped for want of a key. (`attempt doctor` exits
    /// non-zero.) Skipped databases and device repairs are information.
    pub fn has_problem(&self) -> bool {
        self.config_error.is_some()
            || self
                .encryption
                .as_ref()
                .is_some_and(|e| e.state == "withholding")
    }

    /// One line per finding, ready to print under the doctor's header lines
    /// (the wording lives here so the CLI and its tests share it).
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(why) = &self.config_error {
            out.push(format!(
                "config       PROBLEM {why}; only metadata is captured until it is fixed (a copy of the file is kept when `attempt init` rewrites it)"
            ));
        }
        for ignored in &self.ignored_local_databases {
            out.push(format!(
                "local db     ignored {}: {}; events from here go to your own database",
                ignored.path.display(),
                ignored.reason
            ));
        }
        if !self.device_repairs.is_empty() {
            out.push(format!(
                "device       device.json was corrupt and replaced {} time(s); this machine's device id changed (kept: {})",
                self.device_repairs.len(),
                self.device_repairs
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if let Some(enc) = &self.encryption {
            match enc.state.as_str() {
                "withholding" => out.push(format!(
                    "encryption   PROBLEM no key since {}: new events are stored without their content ({} so far); {}{}",
                    enc.since,
                    enc.withheld_events,
                    enc.advice.as_deref().unwrap_or("run `attempt keys status`"),
                    if enc.problems.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", enc.problems.join("; "))
                    }
                )),
                "inline" if !enc.problems.is_empty() => out.push(format!(
                    "encryption   no key ({}); content is stored unencrypted. Run `attempt keys init` to encrypt from the next flush on",
                    enc.problems.join("; ")
                )),
                _ => {}
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::codex_trust::*;
    use super::*;
    use crate::config::EncryptionMode;
    use crate::install::{install_to, planned_config};
    use serde_json::json;
    use std::fs;

    fn health() -> CaptureHealth {
        CaptureHealth {
            config_error: None,
            ignored_local_databases: Vec::new(),
            device_repairs: Vec::new(),
            encryption: None,
        }
    }

    #[test]
    fn capture_health_is_silent_when_nothing_is_wrong() {
        let h = health();
        assert!(h.lines().is_empty());
        assert!(!h.has_problem());
    }

    #[test]
    fn capture_health_words_every_finding() {
        let h = CaptureHealth {
            config_error: Some("config.json: unknown variant `x`".into()),
            ignored_local_databases: vec![crate::locator::IgnoredLocalDb {
                path: "/w/.attemptdb".into(),
                reason: "the spool directory contains a symbolic link".into(),
            }],
            device_repairs: vec!["/d/device.json.corrupt-1".into()],
            encryption: Some(crate::keys::EncryptionState {
                db_id: uuid::Uuid::nil(),
                mode: EncryptionMode::Required,
                state: "withholding".into(),
                since: "2026-10-06T00:00:00Z".into(),
                key_source: None,
                problems: vec!["OS key store unavailable: locked".into()],
                withheld_events: 12,
                advice: Some("run `attempt keys status`".into()),
            }),
        };
        let lines = h.lines();
        assert_eq!(lines.len(), 4, "{lines:#?}");
        assert!(lines[0].starts_with("config") && lines[0].contains("only metadata"));
        assert!(lines[1].contains("/w/.attemptdb") && lines[1].contains("your own database"));
        assert!(lines[2].contains("device.json.corrupt-1"));
        assert!(lines[3].contains("12 so far") && lines[3].contains("locked"));
        assert!(h.has_problem());
        // Ignored databases and repairs are information, not a failing doctor.
        let info = CaptureHealth {
            config_error: None,
            encryption: None,
            ..h
        };
        assert!(!info.has_problem());
    }

    #[test]
    fn activity_requires_recent_real_capture_and_uses_latest_timestamp() {
        let now = Timestamp::parse("2026-09-06T12:00:00Z").unwrap();
        let device = attemptdb_core::DeviceId::nil();
        let mut event = Event::new(
            device,
            attemptdb_core::event::Provider::Codex,
            "Stop",
            EventKind::TurnStopped,
            attemptdb_core::ProjectRef::derive("/home/dev/example/project", None, &device),
            "s",
            attemptdb_core::CaptureMode::MetadataOnly,
            "test",
        );
        event.captured_at = now;
        event.attrs.insert("reconstructed".into(), true.into());
        let mut activity = ActivitySummary::default();
        activity.record(&event);
        assert_eq!(activity_state(Some(&activity), now), HookState::Unverified);
        event.kind = EventKind::CaptureTest;
        activity.record(&event);
        assert_eq!(activity_state(Some(&activity), now), HookState::Verified);
        event.kind = EventKind::TurnStopped;
        event.attrs.remove("reconstructed");
        activity.record(&event);
        event.captured_at = Timestamp::from_micros(now.as_micros() - ACTIVE_WINDOW_US - 1);
        activity.record(&event);
        assert_eq!(activity.event_count, 2);
        assert_eq!(
            activity.last_event_at.as_deref(),
            Some(now.to_rfc3339().as_str())
        );
        assert_eq!(activity_state(Some(&activity), now), HookState::Active);
        let later = Timestamp::from_micros(now.as_micros() + ACTIVE_WINDOW_US + 1);
        assert_eq!(activity_state(Some(&activity), later), HookState::Stale);
    }

    #[test]
    fn disabled_provider_settings_are_not_reported_active() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = fake_binary(tmp.path(), "attempt");
        let activity = Some(ActivitySummary {
            last_event_at: Some(Timestamp::now().to_rfc3339()),
            event_count: 1,
            capture_test_seen: false,
        });
        for kind in [AgentKind::ClaudeCode, AgentKind::Codex] {
            let cfg = tmp.path().join(format!("{}.json", kind.provider_id()));
            let cmd = crate::install::hook_command(&bin, kind);
            let mut config = planned_config(kind, &cmd);
            config["disableAllHooks"] = true.into();
            fs::write(&cfg, serde_json::to_vec(&config).unwrap()).unwrap();
            let toml_path = tmp.path().join("config.toml");
            let text = "[features]\nhooks = false\n";
            fs::write(&toml_path, text).unwrap();
            let diagnosis = diagnose_agent(
                kind,
                None,
                Some(cfg),
                &bin,
                Some(&toml_path),
                activity.clone(),
            );
            assert_eq!(diagnosis.state, HookState::Disabled);
            assert_eq!(
                fs::read_to_string(toml_path).unwrap(),
                text,
                "doctor never writes trust"
            );
        }
    }

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
                last_event_at: Some(Timestamp::now().to_rfc3339()),
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
                last_event_at: Some(Timestamp::now().to_rfc3339()),
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
        // Trust does not override the user's enabled=false choice.
        let text = fs::read_to_string(&toml_path)
            .unwrap()
            .replace("trusted_hash =", "enabled = false\ntrusted_hash =");
        fs::write(&toml_path, &text).unwrap();
        let disabled = diagnose_agent(
            AgentKind::Codex,
            None,
            Some(cfg),
            &bin,
            Some(&toml_path),
            None,
        );
        assert_eq!(disabled.state, HookState::Disabled);
        assert_eq!(fs::read_to_string(toml_path).unwrap(), text);
    }

    fn detected_claude(dir: &Path) -> DetectedAgent {
        DetectedAgent {
            kind: AgentKind::ClaudeCode,
            config_dir: dir.to_path_buf(),
            config_path: dir.join("settings.json"),
            config_exists: dir.join("settings.json").is_file(),
            detected_by: vec!["fixture".into()],
            binary_path: None,
            version: None,
        }
    }

    #[test]
    fn each_claude_config_directory_has_its_own_state() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("attempt");
        fs::write(&bin, b"").unwrap();
        let wired = tmp.path().join(".claude");
        let unwired = tmp.path().join(".claude-acct2");
        let stale = tmp.path().join(".claude-old");
        for d in [&wired, &unwired, &stale] {
            fs::create_dir_all(d).unwrap();
        }
        fs::write(unwired.join("settings.json"), r#"{"model":"opus"}"#).unwrap();
        let cmd = format!("'{}' hook claude-code", bin.display());
        install_to(
            AgentKind::ClaudeCode,
            &wired.join("settings.json"),
            &cmd,
            false,
        )
        .unwrap();
        // Wired once for a binary that has since moved.
        install_to(
            AgentKind::ClaudeCode,
            &stale.join("settings.json"),
            "'/gone/attempt' hook claude-code",
            false,
        )
        .unwrap();
        let dets = [
            detected_claude(&wired),
            detected_claude(&unwired),
            detected_claude(&stale),
        ];
        let refs: Vec<&DetectedAgent> = dets.iter().collect();
        let active = ActivitySummary {
            last_event_at: Some(Timestamp::now().to_rfc3339()),
            event_count: 7,
            capture_test_seen: true,
        };
        let out = diagnose_kind(
            &Scope::User,
            &bin,
            AgentKind::ClaudeCode,
            &refs,
            Some(active),
        );
        assert_eq!(out.len(), 3, "one entry per directory");
        assert_eq!(out[0].state, HookState::Active);
        assert_eq!(out[0].config_dir.as_deref(), Some(wired.as_path()));
        // The directory with no hooks is not "active" because another one is.
        assert_eq!(out[1].state, HookState::NotInstalled, "{:?}", out[1]);
        assert!(
            out[1].notes.iter().any(|n| n.contains("not captured")),
            "{:?}",
            out[1].notes
        );
        assert_eq!(out[2].state, HookState::Stale, "{:?}", out[2]);
        assert!(out[2].activity.is_none(), "events are per provider");

        // One directory: exactly the old shape, activity included.
        let single = diagnose_kind(&Scope::User, &bin, AgentKind::ClaudeCode, &refs[..1], None);
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].state, HookState::Configured);
        // A project scope names one file, whatever was detected.
        let project = diagnose_kind(
            &Scope::Project(tmp.path().to_path_buf()),
            &bin,
            AgentKind::ClaudeCode,
            &refs,
            None,
        );
        assert_eq!(project.len(), 1);
        // Nothing detected: one "not detected" entry.
        let none = diagnose_kind(&Scope::User, &bin, AgentKind::ClaudeCode, &[], None);
        assert_eq!(none.len(), 1);
        assert!(!none[0].detected);
    }

    /// Reads the real machine's agent directories (including the Codex
    /// config) and runs every agent found on PATH with `--version`: a
    /// launcher can create its own state in the working directory. `attempt
    /// doctor` is covered hermetically by `crates/attempt/tests/
    /// capture_health.rs`; run this one by hand with `--ignored`.
    #[test]
    #[ignore = "executes the real agents on PATH and reads their real config"]
    fn whole_machine_diagnosis_does_not_panic() {
        let diag = diagnose(&|_| None);
        // At least one entry per agent; Claude Code has one per config
        // directory, and a machine may have several.
        for kind in AgentKind::ALL {
            assert!(diag.agents.iter().any(|a| a.agent == kind), "{kind}");
        }
    }
}
