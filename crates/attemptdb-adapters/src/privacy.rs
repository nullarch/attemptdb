//! Privacy canary helpers.
//!
//! These functions do not participate in normalisation; they exist so the
//! crate's tests (and any downstream test suite) can prove that content from
//! a payload never reaches metadata fields. They are deliberately simple
//! substring checks: a canary that survives normalisation is a bug, and a
//! false positive is cheap to inspect.

use attemptdb_core::Event;
use serde_json::{Map, Value};

/// Payload keys whose string values (recursively) are content, never
/// metadata.
pub const CONTENT_KEYS: &[&str] = &[
    "prompt",
    "prompt_response",
    "command",
    "content",
    "new_string",
    "old_string",
    "edits",
    "output",
    "tool_response",
    "error",
    "error_message",
    "error_details",
    "last_assistant_message",
    "message",
    "title",
    "description",
    "patch",
    "stdout",
    "stderr",
    "custom_instructions",
    "task_subject",
    "task_description",
    "text",
    "user_email",
    "transcript_path",
    "agent_transcript_path",
];

/// Shortest string treated as a canary. Shorter values (`"x"`, `"ok"`)
/// collide with legitimate metadata too easily to be meaningful.
pub const MIN_CANARY_LEN: usize = 6;

/// Every string that appears under a content key of `payload`, at any
/// depth. Keys starting with `_` are ignored.
pub fn collect_content_strings(payload: &Value) -> Vec<String> {
    let mut out = Vec::new();
    walk(payload, false, &mut out);
    out.sort();
    out.dedup();
    out
}

/// Keys whose bare-token values are provider classifications (`rate_limit`,
/// `timeout`), which adapters legitimately lift into `outcome.class`.
const CLASSIFICATION_KEYS: &[&str] = &["error", "error_type", "failure_type"];

fn walk(value: &Value, under_content: bool, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if key.starts_with('_') || is_classification(key, child) {
                    continue;
                }
                let content = under_content || CONTENT_KEYS.contains(&key.as_str());
                walk(child, content, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                walk(item, under_content, out);
            }
        }
        Value::String(s) if under_content && s.len() >= MIN_CANARY_LEN => out.push(s.clone()),
        _ => {}
    }
}

fn is_classification(key: &str, value: &Value) -> bool {
    CLASSIFICATION_KEYS.contains(&key) && value.as_str().is_some_and(crate::common::is_token)
}

/// The first needle found in `haystack`, matched both verbatim and in its
/// JSON-escaped form so multi-line values are caught in serialised output.
pub fn find_leak<'a>(haystack: &str, needles: &'a [String]) -> Option<&'a str> {
    needles.iter().map(String::as_str).find(|needle| {
        haystack.contains(needle) || haystack.contains(json_escaped(needle).as_str())
    })
}

/// A string as it appears inside a JSON document, without the quotes.
pub fn json_escaped(s: &str) -> String {
    let quoted = serde_json::to_string(s).unwrap_or_default();
    quoted.trim_matches('"').to_string()
}

/// Dotted attribute keys whose string value contains `needle`, skipping the
/// top-level keys in `exempt`.
pub fn attrs_containing(attrs: &Map<String, Value>, needle: &str, exempt: &[&str]) -> Vec<String> {
    let mut hits = Vec::new();
    for (key, value) in attrs {
        if exempt.contains(&key.as_str()) {
            continue;
        }
        collect_hits(key, value, needle, &mut hits);
    }
    hits
}

fn collect_hits(path: &str, value: &Value, needle: &str, hits: &mut Vec<String>) {
    match value {
        Value::String(s) if s.contains(needle) => hits.push(path.to_string()),
        Value::Object(map) => {
            for (k, v) in map {
                collect_hits(&format!("{path}.{k}"), v, needle, hits);
            }
        }
        Value::Array(items) => {
            for (i, v) in items.iter().enumerate() {
                collect_hits(&format!("{path}[{i}]"), v, needle, hits);
            }
        }
        _ => {}
    }
}

/// True when the event carries content or a raw payload.
pub fn has_content_or_raw(event: &Event) -> bool {
    event.content.is_some() || event.raw.is_some()
}

/// True when the retained raw payload references a transcript path.
pub fn raw_has_transcript_path(event: &Event) -> bool {
    fn has(value: &Value) -> bool {
        match value {
            Value::Object(map) => {
                map.keys().any(|k| k.ends_with("transcript_path")) || map.values().any(has)
            }
            Value::Array(items) => items.iter().any(has),
            _ => false,
        }
    }
    event.raw.as_ref().is_some_and(has)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collects_nested_content_only() {
        let payload = serde_json::json!({
            "tool_name": "Edit",
            "tool_input": {"file_path": "/p/a.ts", "old_string": "SECRET_OLD", "edits": [{"new_string": "SECRET_NEW"}]},
            "prompt": "tell me a secret",
            "_note": "IGNORED_NOTE",
            "short": {"content": "ab"}
        });
        let strings = collect_content_strings(&payload);
        assert_eq!(
            strings,
            vec!["SECRET_NEW", "SECRET_OLD", "tell me a secret"]
        );
    }

    #[test]
    fn finds_leaks_in_attrs() {
        let attrs: Map<String, Value> = serde_json::from_str(
            r#"{"cwd":"/home/dev/p","provider":{"note":"/home/dev/x"},"n":1}"#,
        )
        .unwrap();
        assert_eq!(
            attrs_containing(&attrs, "/home/dev", &["cwd"]),
            vec!["provider.note"]
        );
        let needles = vec!["needle".to_string(), "two\nlines".to_string()];
        assert_eq!(find_leak("hay needle hay", &needles), Some("needle"));
        assert_eq!(
            find_leak(r#"{"x":"two\nlines"}"#, &needles),
            Some("two\nlines")
        );
        assert_eq!(find_leak("hay", &needles), None);
    }

    #[test]
    fn bare_error_tokens_are_classifications() {
        let payload = serde_json::json!({"error": "rate_limit", "error_details": "Rate limit hit"});
        assert_eq!(collect_content_strings(&payload), vec!["Rate limit hit"]);
        let payload = serde_json::json!({"error": "String to replace not found"});
        assert_eq!(
            collect_content_strings(&payload),
            vec!["String to replace not found"]
        );
    }
}
