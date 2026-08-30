//! The `Event.attrs` contract (RFC 0006 §4): which keys may exist and what
//! their values may look like.
//!
//! Adapters are written against this list, and the canary tests prove they
//! honour it — but a test is a promise, not a guarantee. `sanitise` is the
//! guarantee: the single writer applies it to every event at ingestion, so
//! an event that arrived over the network, from an older client, or from a
//! buggy adapter still cannot carry content in its metadata. Each drop is
//! counted in `attrs.redactions`, the one key ingestion itself may write.

use serde_json::{Map, Value};

/// Every key an event may carry in `attrs`. Union of RFC 0006 §4.1 and the
/// keys the adapters, the transcript importer, the correction commands and
/// the sync client emit. Values are names, counts, booleans, classifications
/// and home-elided logical paths — never prompt, command, output or file
/// text.
pub const ALLOWED_ATTR_KEYS: &[&str] = &[
    // RFC 0006 §4.1
    "tool_input_bytes",
    "tool_output_bytes",
    "prompt_chars",
    "message_chars",
    "file_count",
    "line_count",
    "exit_code",
    "duration_ms",
    "permission_mode",
    "permission_decision",
    "notification_type",
    "stop_reason",
    "compaction_trigger",
    "task_status",
    "subagent_type",
    "model",
    "cwd_logical",
    "capture_gap",
    "consent_version",
    "coverage_grade",
    "git_dirty",
    "path_extensions",
    "matcher",
    "hook_event_name",
    // Hook adapters (`attemptdb-adapters`)
    "source",
    "reason",
    "trigger",
    "stop_hook_active",
    "cwd",
    "transcript_present",
    "prompt_lines",
    "prompt_has_code_fence",
    "prompt_has_question",
    "prompt_kind",
    "prompt_source",
    "command_bytes",
    "command_category",
    "command_subcategory",
    "git_subcommand",
    "file_ext",
    "file_is_test",
    "file_is_config",
    "file_is_doc",
    "lines_added",
    "lines_removed",
    "edit_count",
    "attachment_count",
    "image_count",
    "error_class",
    "error_bytes",
    "is_subagent",
    "agent_type",
    "task_id",
    "previous_cwd",
    "worktree_path",
    "config_source",
    "tool_output_truncated",
    "entrypoint",
    "pre_tokens",
    "post_tokens",
    "output_tokens",
    // Provider-specific scalars, nested under one object (`provider_attr`).
    "provider",
    // Set by the hook when an adapter fails and the raw event is kept.
    "adapter_error",
    // Transcript import (reconstructed history).
    "reconstructed",
    "reconstructed_from",
    "transcript_entry_type",
    "is_sidechain",
    "turn_index_hint",
    // Corrections and retractions (RFC 0003 §8).
    "correction_type",
    "target",
    "target_type",
    "outcome",
    "note_chars",
    // Capture timing written by the hook / benchmark harness.
    "hook_us",
    // Sync (RFC 0006 §10): the uploading device's own sequence number.
    "device_seq",
    // Written by ingestion itself.
    REDACTIONS_KEY,
];

/// The one key ingestion writes: how many attrs were dropped from this
/// event because they failed the contract.
pub const REDACTIONS_KEY: &str = "redactions";

/// Longest string an attr value may be. Metadata is short; anything longer
/// is text in disguise.
pub const MAX_ATTR_STRING: usize = 256;

/// Whether `key` may appear in `attrs`: allowlisted, or a provider extension
/// of the form `x_<provider>_<name>` (lower-case ASCII, digits, underscores).
pub fn key_allowed(key: &str) -> bool {
    ALLOWED_ATTR_KEYS.contains(&key) || is_provider_extension(key)
}

fn is_provider_extension(key: &str) -> bool {
    let Some(rest) = key.strip_prefix("x_") else {
        return false;
    };
    if !rest
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return false;
    }
    // `x_<provider>_<name>`: both segments non-empty.
    match rest.split_once('_') {
        Some((provider, name)) => !provider.is_empty() && !name.is_empty(),
        None => false,
    }
}

/// Whether a string value satisfies RFC 0006 §4.3: short, single-line, not
/// an email address, not a home-directory path, not a secret (§5).
pub fn value_allowed(s: &str) -> bool {
    s.len() <= MAX_ATTR_STRING
        && !s.contains(['\n', '\r'])
        && !looks_like_email(s)
        && !has_home_directory(s)
        && !crate::secrets::contains_secret(s)
}

fn looks_like_email(s: &str) -> bool {
    let Some(at) = s.find('@') else {
        return false;
    };
    let (local, domain) = (&s[..at], &s[at + 1..]);
    let local_ok = !local.is_empty()
        && local
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._%+-".contains(&b));
    let domain_ok = domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && domain
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    local_ok && domain_ok
}

/// `/Users/<name>/`, `/home/<name>/`, `C:\Users\<name>\` (any drive, either
/// separator), anywhere in the value. Logical paths (`~/proj`) pass.
fn has_home_directory(s: &str) -> bool {
    if s.contains("/Users/") || s.contains("/home/") {
        return true;
    }
    let b = s.as_bytes();
    for i in 0..b.len().saturating_sub(8) {
        if b[i].is_ascii_alphabetic()
            && b[i + 1] == b':'
            && (b[i + 2] == b'\\' || b[i + 2] == b'/')
            && s[i + 3..].starts_with("Users")
            && matches!(b.get(i + 8), Some(b'\\') | Some(b'/'))
        {
            return true;
        }
    }
    false
}

/// Enforce the contract on one `attrs` map in place. Returns how many
/// entries were dropped; when non-zero, `attrs.redactions` is incremented
/// by that amount.
///
/// Rules:
/// - an unknown key is dropped;
/// - a string that fails [`value_allowed`] is dropped;
/// - an array is dropped if any string element fails;
/// - an object is only allowed under `provider`, whose entries are checked
///   individually;
/// - numbers, booleans and null are always fine.
pub fn sanitise(attrs: &mut Map<String, Value>) -> usize {
    let mut dropped = 0;
    let keys: Vec<String> = attrs.keys().cloned().collect();
    for key in keys {
        if !key_allowed(&key) {
            attrs.remove(&key);
            dropped += 1;
            continue;
        }
        let remove = match attrs.get_mut(&key) {
            Some(Value::String(s)) => !value_allowed(s),
            Some(Value::Array(items)) => items
                .iter()
                .any(|v| matches!(v, Value::String(s) if !value_allowed(s)) || v.is_object()),
            Some(Value::Object(inner)) if key == "provider" => {
                dropped += sanitise_provider(inner);
                false
            }
            Some(Value::Object(_)) => true,
            Some(Value::Number(_) | Value::Bool(_) | Value::Null) | None => false,
        };
        if remove {
            attrs.remove(&key);
            dropped += 1;
        }
    }
    if dropped > 0 {
        let prior = attrs
            .get(REDACTIONS_KEY)
            .and_then(Value::as_u64)
            .unwrap_or(0);
        attrs.insert(
            REDACTIONS_KEY.to_string(),
            Value::from(prior + dropped as u64),
        );
    }
    dropped
}

/// The nested provider object: scalar values only, checked by the same
/// string rules. Keys are provider field names and are not allowlisted.
fn sanitise_provider(inner: &mut Map<String, Value>) -> usize {
    let bad: Vec<String> = inner
        .iter()
        .filter(|(_, v)| match v {
            Value::String(s) => !value_allowed(s),
            Value::Number(_) | Value::Bool(_) | Value::Null => false,
            Value::Array(_) | Value::Object(_) => true,
        })
        .map(|(k, _)| k.clone())
        .collect();
    for k in &bad {
        inner.remove(k);
    }
    bad.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn map(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap()
    }

    #[test]
    fn allowlisted_metadata_survives_untouched() {
        let mut a = map(json!({
            "hook_event_name": "PostToolUse", "prompt_chars": 42, "file_is_test": true,
            "cwd": "~/proj", "path_extensions": ["rs", "md"],
            "provider": {"tool_use_id": "toolu_01", "n": 3}
        }));
        let before = a.clone();
        assert_eq!(sanitise(&mut a), 0);
        assert_eq!(a, before);
        assert!(!a.contains_key(REDACTIONS_KEY));
    }

    #[test]
    fn unknown_keys_are_dropped_and_counted() {
        let mut a = map(json!({"prompt": "please rewrite auth.ts", "prompt_chars": 22}));
        assert_eq!(sanitise(&mut a), 1);
        assert!(!a.contains_key("prompt"));
        assert_eq!(a["prompt_chars"], 22);
        assert_eq!(a[REDACTIONS_KEY], 1);
    }

    #[test]
    fn provider_extensions_follow_the_pattern() {
        assert!(key_allowed("x_codex_turn_kind"));
        assert!(key_allowed("x_gemini_cli_mode"));
        assert!(!key_allowed("x_"));
        assert!(!key_allowed("x_codex"));
        assert!(!key_allowed("x_Codex_kind"));
        assert!(!key_allowed("xcodex_kind"));
    }

    #[test]
    fn content_shaped_values_are_dropped() {
        let long = "a".repeat(MAX_ATTR_STRING + 1);
        let mut a = map(json!({
            "reason": long,
            "source": "line one\nline two",
            "agent_type": "dev@example.com",
            "cwd": "/Users/chung/streamize/attemptdb",
            "previous_cwd": "C:\\Users\\chung\\proj",
            "worktree_path": "~/proj/.worktrees/a",
            "path_extensions": ["rs", "/home/dev/leak"],
            "provider": {"ok": "short", "leak": "/home/dev/secret", "obj": {"x": 1}}
        }));
        let dropped = sanitise(&mut a);
        assert_eq!(dropped, 8, "{a:?}");
        for gone in [
            "reason",
            "source",
            "agent_type",
            "cwd",
            "previous_cwd",
            "path_extensions",
        ] {
            assert!(!a.contains_key(gone), "{gone} survived");
        }
        assert_eq!(a["worktree_path"], "~/proj/.worktrees/a");
        assert_eq!(a["provider"], json!({"ok": "short"}));
        assert_eq!(a[REDACTIONS_KEY], 8);
    }

    #[test]
    fn secrets_in_attrs_are_dropped() {
        let mut a = map(json!({
            "reason": "token ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdef0123 leaked",
            "source": "startup"
        }));
        assert_eq!(sanitise(&mut a), 1);
        assert!(!a.contains_key("reason"));
        assert_eq!(a["source"], "startup");
    }

    #[test]
    fn objects_only_under_provider() {
        let mut a = map(json!({"reason": {"nested": "x"}, "provider": {"a": 1}}));
        assert_eq!(sanitise(&mut a), 1);
        assert!(!a.contains_key("reason"));
        assert_eq!(a["provider"], json!({"a": 1}));
    }

    #[test]
    fn redactions_accumulate_across_passes() {
        let mut a = map(json!({"prompt": "x", REDACTIONS_KEY: 2}));
        assert_eq!(sanitise(&mut a), 1);
        assert_eq!(a[REDACTIONS_KEY], 3);
        // A second pass over clean attrs changes nothing.
        assert_eq!(sanitise(&mut a), 0);
        assert_eq!(a[REDACTIONS_KEY], 3);
    }

    #[test]
    fn email_and_home_heuristics() {
        assert!(looks_like_email("a.b+c@example.co"));
        assert!(!looks_like_email("@example"));
        assert!(!looks_like_email("not an email @ all"));
        assert!(has_home_directory("D:/Users/x/y"));
        assert!(has_home_directory("prefix /home/x"));
        assert!(!has_home_directory("~/proj/src"));
        assert!(!has_home_directory("C:/Program Files/x"));
    }
}
