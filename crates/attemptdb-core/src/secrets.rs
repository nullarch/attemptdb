//! Secret detection (RFC 0006 §5).
//!
//! High-precision rules only: every pattern here identifies a credential by
//! its issuer's own format, so a match is a secret with near certainty and
//! ordinary code, paths, and prose do not trip it. The ruleset is versioned
//! (`RULESET`) because a match is recorded as the reason an attr was dropped
//! or a content span redacted, and a later ruleset may decide differently.
//!
//! Where it applies:
//! - `attrs` values at ingestion: a value that contains a secret is dropped
//!   (via [`crate::attrs::value_allowed`]).
//! - content before it leaves the device with `--send-content`, and in
//!   sanitised exports: the span is replaced by `[REDACTED:<rule>]`.
//!
//! No regex dependency: each rule is a small hand-written scanner.

use serde_json::Value;

pub const RULESET: &str = "secrets-v1";

/// One detected span.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub rule: &'static str,
    pub start: usize,
    pub end: usize,
}

fn is_b64ish(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'+' || b == b'/' || b == b'='
}

/// Longest run of `pred` bytes starting at `i`.
fn run(s: &[u8], i: usize, pred: fn(u8) -> bool) -> usize {
    let mut j = i;
    while j < s.len() && pred(s[j]) {
        j += 1;
    }
    j - i
}

/// Prefix-based token formats: (rule, literal prefix, minimum tail length,
/// tail predicate). The tail must be a run of token characters at least
/// `min` long and must end at a non-token byte.
const PREFIXED: &[(&str, &str, usize)] = &[
    ("aws_access_key_id", "AKIA", 16),
    ("aws_access_key_id", "ASIA", 16),
    ("github_token", "ghp_", 36),
    ("github_token", "gho_", 36),
    ("github_token", "ghu_", 36),
    ("github_token", "ghs_", 36),
    ("github_token", "ghr_", 36),
    ("github_token", "github_pat_", 22),
    ("slack_token", "xoxb-", 10),
    ("slack_token", "xoxp-", 10),
    ("slack_token", "xoxa-", 10),
    ("slack_token", "xoxr-", 10),
    ("google_api_key", "AIza", 35),
    ("stripe_key", "sk_live_", 20),
    ("stripe_key", "sk_test_", 20),
    ("stripe_key", "rk_live_", 20),
    ("anthropic_api_key", "sk-ant-", 20),
    ("openai_api_key", "sk-proj-", 20),
    ("npm_token", "npm_", 36),
    ("supabase_service_key", "sbp_", 20),
    ("vercel_token", "vercel_", 20),
];

// Assembled at compile time so this file never contains a contiguous PEM
// marker: the repository's own pre-commit private-key scan would otherwise
// flag the detector for the thing it detects.
const PEM_MARKERS: &[&str] = &[
    concat!("-----BEGIN ", "RSA PRIVATE KEY-----"),
    concat!("-----BEGIN ", "EC PRIVATE KEY-----"),
    concat!("-----BEGIN ", "DSA PRIVATE KEY-----"),
    concat!("-----BEGIN ", "OPENSSH PRIVATE KEY-----"),
    concat!("-----BEGIN ", "PRIVATE KEY-----"),
    concat!("-----BEGIN ", "ENCRYPTED PRIVATE KEY-----"),
    concat!("-----BEGIN ", "PGP PRIVATE KEY BLOCK-----"),
];

/// Every secret span in `text`, non-overlapping, in order.
pub fn scan(text: &str) -> Vec<Hit> {
    let b = text.as_bytes();
    let mut hits: Vec<Hit> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if let Some(h) = at(text, b, i) {
            i = h.end;
            hits.push(h);
        } else {
            i += 1;
        }
    }
    hits
}

fn at(text: &str, b: &[u8], i: usize) -> Option<Hit> {
    // A token must start a word: preceded by nothing or a non-identifier
    // byte (`=`, `:`, quotes and spaces all end a word; `x_ghp_…` does not).
    if i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_') {
        return None;
    }
    for (rule, prefix, min) in PREFIXED {
        if text[i..].starts_with(prefix) {
            let tail = run(b, i + prefix.len(), is_b64ish);
            if tail >= *min {
                return Some(Hit {
                    rule,
                    start: i,
                    end: i + prefix.len() + tail,
                });
            }
        }
    }
    if b[i] == b'-' {
        for marker in PEM_MARKERS {
            if text[i..].starts_with(marker) {
                // Redact through the matching END marker when present, else
                // to the end of the text.
                let end_marker = marker.replace("BEGIN", "END");
                let end = text[i..]
                    .find(&end_marker)
                    .map(|p| i + p + end_marker.len())
                    .unwrap_or(text.len());
                return Some(Hit {
                    rule: "private_key",
                    start: i,
                    end,
                });
            }
        }
    }
    // JWT: three base64url segments, the first two decoding to JSON is not
    // checked; the `eyJ` header start ("{\"") plus two dots and length is
    // specific enough.
    if text[i..].starts_with("eyJ") {
        let seg1 = run(b, i, |c| {
            c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
        });
        let mut j = i + seg1;
        if seg1 >= 10 && j < b.len() && b[j] == b'.' && text[j + 1..].starts_with("eyJ") {
            let seg2 = run(b, j + 1, |c| {
                c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
            });
            j += 1 + seg2;
            if seg2 >= 10 && j < b.len() && b[j] == b'.' {
                let seg3 = run(b, j + 1, |c| {
                    c.is_ascii_alphanumeric() || c == b'_' || c == b'-'
                });
                if seg3 >= 10 {
                    return Some(Hit {
                        rule: "jwt",
                        start: i,
                        end: j + 1 + seg3,
                    });
                }
            }
        }
    }
    None
}

/// True when `text` contains at least one secret.
pub fn contains_secret(text: &str) -> bool {
    let b = text.as_bytes();
    (0..b.len()).any(|i| at(text, b, i).is_some())
}

/// `text` with every secret span replaced by `[REDACTED:<rule>]`.
pub fn redact(text: &str) -> (String, usize) {
    let hits = scan(text);
    if hits.is_empty() {
        return (text.to_string(), 0);
    }
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for h in &hits {
        out.push_str(&text[last..h.start]);
        out.push_str("[REDACTED:");
        out.push_str(h.rule);
        out.push(']');
        last = h.end;
    }
    out.push_str(&text[last..]);
    (out, hits.len())
}

/// Redact every string inside a JSON value, in place. Returns the number of
/// spans redacted.
pub fn redact_value(v: &mut Value) -> usize {
    match v {
        Value::String(s) => {
            let (r, n) = redact(s);
            if n > 0 {
                *s = r;
            }
            n
        }
        Value::Array(items) => items.iter_mut().map(redact_value).sum(),
        Value::Object(map) => map.values_mut().map(redact_value).sum(),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issuer_formats_are_detected_and_prose_is_not() {
        let secrets = [
            ("AKIAIOSFODNN7EXAMPLE", "aws_access_key_id"),
            ("ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdef0123", "github_token"),
            ("github_pat_11ABCDEFG0123456789abcdefghij", "github_token"),
            ("xoxb-1234567890-abcdefghijkl", "slack_token"),
            ("AIzaSyA1234567890abcdefghijklmnopqrstuv", "google_api_key"),
            ("sk_live_51H1234567890abcdefghijkl", "stripe_key"),
            (
                "sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789",
                "anthropic_api_key",
            ),
            (
                "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U",
                "jwt",
            ),
        ];
        for (s, rule) in secrets {
            let hits = scan(&format!("token={s} rest"));
            assert_eq!(hits.len(), 1, "{s}");
            assert_eq!(hits[0].rule, rule, "{s}");
            assert!(contains_secret(s));
        }
        for benign in [
            "cargo test -p attemptdb-core",
            "AKIA is a prefix, not a key",
            "ghp_short",
            "the sky is blue",
            "src/lib.rs:42",
            "https://github.com/nullarch/attemptdb",
            "eyJ.eyJ.x",
        ] {
            assert!(!contains_secret(benign), "{benign}");
        }
    }

    #[test]
    fn private_keys_are_redacted_whole() {
        let text = concat!(
            "config:\n-----BEGIN ",
            "OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAA\n-----END ",
            "OPENSSH PRIVATE KEY-----\ndone"
        );
        let (r, n) = redact(text);
        assert_eq!(n, 1);
        assert_eq!(r, "config:\n[REDACTED:private_key]\ndone");
    }

    #[test]
    fn redaction_keeps_everything_else() {
        let (r, n) = redact("export TOKEN=ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdef0123 && echo ok");
        assert_eq!(n, 1);
        assert_eq!(r, "export TOKEN=[REDACTED:github_token] && echo ok");
        let mut v = serde_json::json!({"cmd": "curl -H 'Authorization: Bearer sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789'", "n": 1, "list": ["AKIAIOSFODNN7EXAMPLE"]});
        assert_eq!(redact_value(&mut v), 2);
        assert!(!v.to_string().contains("sk-ant-"));
        assert!(v.to_string().contains("[REDACTED:aws_access_key_id]"));
    }
}
