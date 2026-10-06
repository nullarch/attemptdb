//! Secret detection (RFC 0006 §5).
//!
//! Precision first: ordinary code, paths, hashes, ids and prose must not trip
//! a rule, because a false positive silently damages the record while a miss
//! is the documented limit of a pattern scanner (RFC 0006 §5 says so). Two
//! families of rules:
//!
//! - **Issuer formats** (`aws_access_key_id`, `github_token`, `slack_token`,
//!   `anthropic_api_key`, `openai_api_key`, `jwt`, `private_key`, …): the
//!   credential identifies itself, so a match is a secret with near certainty.
//! - **Structural rules** (`generic_assignment`, `url_credentials`,
//!   `authorization_header`, `aws_secret_key`): the credential is recognised by
//!   where it sits — the value of `password=`, `"token": "…"`, `--password …`,
//!   `scheme://user:pass@host`, `Authorization: Bearer …`, or next to an AWS
//!   secret label. A structural rule fires only when the *name* says secret
//!   and the *value* is shaped like one (not a variable, a type, a call, a
//!   placeholder, a number or an ordinary lowercase word). It does not claim
//!   to find every password: `password = hunter` in prose is not found.
//!
//! The ruleset is versioned (`RULESET`) because a match is recorded as the
//! reason an attr was dropped or a content span redacted, and a later ruleset
//! may decide differently.
//!
//! Where it applies:
//! - `attrs` values at ingestion: a value that contains a secret is dropped
//!   (via [`crate::attrs::value_allowed`]).
//! - content before it leaves the device (every sync profile that sends text),
//!   and in sanitised exports: the span is replaced by `[REDACTED:<rule>]`.
//!   [`redact_event_content`] is the one entry point for an event.
//! - Local persistence is **not** covered unless the capture ingest path calls
//!   [`redact_event_content`]; see RFC 0006 §5.
//!
//! No regex dependency: each rule is a small hand-written scanner.

use crate::event::Event;
use serde_json::Value;
use std::collections::BTreeMap;

pub const RULESET: &str = "secrets-v2";

/// One detected span.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub rule: &'static str,
    pub start: usize,
    pub end: usize,
}

/// What a redaction pass over one event (or one value) did. Counts only:
/// never the secret and never its position.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RedactionStats {
    /// Spans replaced by `[REDACTED:<rule>]`.
    pub spans: usize,
    /// Values (strings, JSON members) that changed.
    pub fields: usize,
    /// Spans per rule id.
    pub by_rule: BTreeMap<&'static str, usize>,
}

impl RedactionStats {
    pub fn is_empty(&self) -> bool {
        self.spans == 0
    }

    fn record(&mut self, rule: &'static str) {
        self.spans += 1;
        *self.by_rule.entry(rule).or_default() += 1;
    }

    /// Fold another pass into this one.
    pub fn absorb(&mut self, other: RedactionStats) {
        self.spans += other.spans;
        self.fields += other.fields;
        for (rule, n) in other.by_rule {
            *self.by_rule.entry(rule).or_default() += n;
        }
    }
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

/// Labels that introduce an AWS secret access key (matched case-insensitively
/// on a word start). The value after one is a 40-character base64 string.
const AWS_SECRET_LABELS: &[&str] = &[
    "aws_secret_access_key",
    "aws_secret_key",
    "awssecretaccesskey",
    "secretaccesskey",
    "secret_access_key",
];

/// Longest identifier the structural rules read: a name longer than this is
/// not a variable name, and the cap keeps the scan linear on hostile input.
const MAX_IDENT: usize = 64;

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

fn starts_with_ci(b: &[u8], i: usize, lit: &str) -> bool {
    let l = lit.len();
    i + l <= b.len() && b[i..i + l].eq_ignore_ascii_case(lit.as_bytes())
}

fn at(text: &str, b: &[u8], i: usize) -> Option<Hit> {
    // The scan walks bytes, so `i` can land inside a multi-byte character
    // (any prose that is not ASCII). No rule starts there — every prefix is
    // ASCII — and slicing there would panic.
    if !text.is_char_boundary(i) {
        return None;
    }
    // Credentials in a URL's userinfo are found from the `://` (the scheme
    // before it is a word, so the word-start gate below would refuse).
    if b[i] == b':' && text[i..].starts_with("://") {
        return url_credentials(text, b, i);
    }
    // A token must start a word: preceded by nothing or a non-identifier
    // byte (`=`, `:`, quotes and spaces all end a word; `x_ghp_…` does not).
    if i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_') {
        return None;
    }
    token_at(text, b, i)
        .or_else(|| authorization_header(text, b, i))
        .or_else(|| aws_secret_key(text, b, i))
        .or_else(|| generic_assignment(text, b, i))
}

/// The issuer-format rules: a credential that identifies itself.
fn token_at(text: &str, b: &[u8], i: usize) -> Option<Hit> {
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
    // The legacy OpenAI key: `sk-` and one unbroken run of at least 32
    // letters and digits with at least one of each. A kebab-case identifier
    // (`sk-learn-…`) has hyphens inside the run and does not qualify.
    if text[i..].starts_with("sk-") {
        let tail = run(b, i + 3, |c| c.is_ascii_alphanumeric());
        let end = i + 3 + tail;
        let t = &b[i + 3..end];
        if (32..=200).contains(&tail)
            && !matches!(b.get(end), Some(b'-') | Some(b'_'))
            && t.iter().any(u8::is_ascii_digit)
            && t.iter().any(u8::is_ascii_alphabetic)
        {
            return Some(Hit {
                rule: "openai_api_key",
                start: i,
                end,
            });
        }
    }
    None
}

/// `Authorization: Bearer <token>` / `Basic <b64>` (any case, quoted JSON
/// forms too). Only the credential is redacted; the header name stays.
fn authorization_header(text: &str, b: &[u8], i: usize) -> Option<Hit> {
    const LABEL: &str = "authorization";
    if !starts_with_ci(b, i, LABEL) {
        return None;
    }
    let mut j = i + LABEL.len();
    if j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
        return None;
    }
    let mut skipped = 0;
    while j < b.len()
        && skipped < 8
        && matches!(b[j], b' ' | b'\t' | b'"' | b'\'' | b'\\' | b':' | b'=')
    {
        j += 1;
        skipped += 1;
    }
    let scheme = if starts_with_ci(b, j, "bearer") {
        6
    } else if starts_with_ci(b, j, "basic") {
        5
    } else {
        return None;
    };
    let k = j + scheme;
    if k >= b.len() || !matches!(b[k], b' ' | b'\t') {
        return None;
    }
    let mut v = k;
    while v < b.len() && matches!(b[v], b' ' | b'\t') {
        v += 1;
    }
    // An issuer-format token keeps its own rule id.
    if let Some(h) = token_at(text, b, v) {
        return Some(h);
    }
    let len = run(b, v, |c| {
        c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'~' | b'+' | b'/' | b'=' | b'-')
    });
    let token = &text[v..v + len];
    // `Bearer authentication scheme` in prose is not a credential; a long
    // unbroken run is, even without a digit.
    if len < 8 || !(secret_shaped(token) || len >= 24) {
        return None;
    }
    Some(Hit {
        rule: "authorization_header",
        start: v,
        end: v + len,
    })
}

/// A 40-character base64 value right after an AWS secret-key label.
fn aws_secret_key(text: &str, b: &[u8], i: usize) -> Option<Hit> {
    let label = AWS_SECRET_LABELS.iter().find(|l| starts_with_ci(b, i, l))?;
    let mut j = i + label.len();
    if j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
        return None;
    }
    let mut skipped = 0;
    while j < b.len()
        && skipped < 8
        && matches!(b[j], b' ' | b'\t' | b'"' | b'\'' | b'\\' | b':' | b'=')
    {
        j += 1;
        skipped += 1;
    }
    let len = run(b, j, |c| {
        c.is_ascii_alphanumeric() || c == b'/' || c == b'+' || c == b'='
    });
    if len == 40 && text.is_char_boundary(j) && text.is_char_boundary(j + len) {
        Some(Hit {
            rule: "aws_secret_key",
            start: j,
            end: j + len,
        })
    } else {
        None
    }
}

/// `scheme://user:password@host`: the whole `user:password` userinfo is
/// replaced, the scheme and host stay. `i` is the `:` of `://`.
fn url_credentials(text: &str, b: &[u8], i: usize) -> Option<Hit> {
    let mut s = i;
    while s > 0
        && i - s < 16
        && (b[s - 1].is_ascii_alphanumeric() || matches!(b[s - 1], b'+' | b'.' | b'-'))
    {
        s -= 1;
    }
    if s == i || !b[s].is_ascii_alphabetic() {
        return None;
    }
    let a = i + 3;
    let mut at_pos = None;
    let mut e = a;
    while e < b.len() && e - a < 512 {
        match b[e] {
            b'/' | b'?' | b'#' | b' ' | b'\t' | b'\n' | b'\r' | b'"' | b'\'' | b'<' | b'>'
            | b'`' | b')' | b']' | b'}' | b'\\' => break,
            b'@' => at_pos = Some(e),
            _ => {}
        }
        e += 1;
    }
    let at_pos = at_pos?;
    let (_, pass) = text[a..at_pos].split_once(':')?;
    if pass.is_empty() || is_placeholder(pass) || is_reference(pass) {
        return None;
    }
    Some(Hit {
        rule: "url_credentials",
        start: a,
        end: at_pos,
    })
}

fn is_ident_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.')
}

/// Whether an identifier names a secret: it must END in a sensitive word
/// (`DB_PASSWORD`, `api_key`, `clientSecret`, `x-api-key`), so `token_count`,
/// `max_tokens`, `secret_name`, `password_field` and `tokenizer` do not.
fn is_secret_name(ident: &str) -> bool {
    let mut parts: Vec<String> = Vec::new();
    for chunk in ident.split(['_', '-', '.']) {
        if chunk.is_empty() {
            continue;
        }
        let mut cur = String::new();
        let mut prev: Option<char> = None;
        for ch in chunk.chars() {
            // camelCase boundary: `apiKey` → api, key.
            if let Some(p) = prev
                && ch.is_ascii_uppercase()
                && (p.is_ascii_lowercase() || p.is_ascii_digit())
            {
                parts.push(std::mem::take(&mut cur).to_ascii_lowercase());
            }
            cur.push(ch);
            prev = Some(ch);
        }
        parts.push(cur.to_ascii_lowercase());
    }
    let n = parts.len();
    let Some(last) = parts.last().map(String::as_str) else {
        return false;
    };
    // `NEXT_PUBLIC_API_KEY`, `publicKeyToken`: public by name.
    if parts.iter().any(|p| p == "public") {
        return false;
    }
    let prev = if n >= 2 {
        Some(parts[n - 2].as_str())
    } else {
        None
    };
    match last {
        "password" | "passwd" | "passphrase" | "secret" | "token" | "apikey" | "secretkey"
        | "accesskey" | "privatekey" | "authtoken" | "clientsecret" => true,
        // `PWD` alone is the working directory and `pass` alone is a verb.
        "pwd" | "pass" => n >= 2,
        "key" => matches!(
            prev,
            Some("api") | Some("secret") | Some("access") | Some("private") | Some("auth")
        ),
        _ => false,
    }
}

/// Characters that end an unquoted value.
fn is_value_delim(c: u8) -> bool {
    c.is_ascii_whitespace()
        || matches!(
            c,
            b'"' | b'\''
                | b'`'
                | b','
                | b';'
                | b'&'
                | b'|'
                | b'<'
                | b'>'
                | b'('
                | b')'
                | b'{'
                | b'}'
                | b'['
                | b']'
                | b'\\'
        )
}

/// Words and masks that stand where a value would be, not values.
fn is_placeholder(v: &str) -> bool {
    let t = v.trim();
    if t.is_empty() {
        return true;
    }
    let l = t.to_ascii_lowercase();
    if matches!(
        l.as_str(),
        "null"
            | "none"
            | "nil"
            | "true"
            | "false"
            | "undefined"
            | "string"
            | "str"
            | "bool"
            | "boolean"
            | "number"
            | "int"
            | "integer"
            | "required"
            | "optional"
            | "redacted"
            | "masked"
            | "empty"
            | "todo"
            | "tbd"
            | "value"
            | "example"
    ) {
        return true;
    }
    if l.starts_with("your_")
        || l.starts_with("your-")
        || l.starts_with('<')
        || l.starts_with("[redacted")
        // An elision in documentation: `"token": "pair_…"`, `KEY=…`.
        || l.starts_with('…')
        || l.ends_with('…')
        || l.ends_with("...")
    {
        return true;
    }
    // An environment variable's name standing for its value:
    // `YOUR_API_KEY`, `OAUTH2_TOKEN`.
    if t.len() >= 3
        && t.as_bytes()[0].is_ascii_uppercase()
        && t.contains('_')
        && t.bytes()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_')
    {
        return true;
    }
    // A mask: one repeated filler character.
    let first = t.as_bytes()[0];
    t.len() >= 3
        && matches!(first, b'*' | b'x' | b'X' | b'.' | b'-' | b'_' | b'#')
        && t.bytes().all(|c| c == first)
}

/// A value that names another value rather than being one: `$TOKEN`,
/// `${TOKEN}`, `%TOKEN%`, `{{ secrets.X }}`, or a pointer type (`*const T`).
fn is_reference(v: &str) -> bool {
    matches!(
        v.as_bytes().first(),
        Some(b'$') | Some(b'%') | Some(b'{') | Some(b'*')
    )
}

/// `settings.api_key`, `process.env.TOKEN`, `node.dot2_token`: attribute
/// access, not a literal. Every segment is an identifier.
fn dotted_identifier(v: &str) -> bool {
    v.contains('.')
        && v.split('.').all(|seg| {
            seg.as_bytes()
                .first()
                .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_')
                && seg.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
        })
}

/// Keywords that declare a variable, so what follows `name =` is an
/// expression, not a literal.
fn declared_in_code(text: &str, i: usize) -> bool {
    let before = text[..i].trim_end_matches([' ', '\t']);
    let word = before
        .rsplit(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .next()
        .unwrap_or("");
    matches!(
        word,
        "let" | "const" | "var" | "mut" | "val" | "final" | "static" | "pub" | "def"
    )
}

/// Shaped like a credential and not like a word: long enough, and with a
/// digit or a symbol in it (`_`, `-`, `.` join words, so they do not count —
/// `user_password` is a variable, `s3cr3t!` is not).
fn secret_shaped(v: &str) -> bool {
    v.len() >= 4
        && !is_placeholder(v)
        && !is_reference(v)
        && v.bytes().any(|c| {
            c.is_ascii_digit() || !(c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
        })
}

/// The value of a secret-named assignment: `KEY=value`, `key: value`,
/// `"key": "value"`, `--password value`.
///
/// What is redacted depends on how sure the form is:
/// - a quoted literal, or `KEY=value` with no spaces around the `=` (env
///   files, query strings, shell), is redacted unless it is a placeholder or
///   a reference;
/// - `key: value`, `key = value` (YAML, code, headers) and `--flag value`
///   additionally need a value shaped like a credential, so `token: string`,
///   `password = user_input` and `--token to` stay;
/// - a value that is itself an issuer-format token or a URL is left to its
///   own rule, which names it properly.
fn generic_assignment(text: &str, b: &[u8], i: usize) -> Option<Hit> {
    if !b[i].is_ascii_alphabetic() {
        return None;
    }
    let mut end = i;
    while end < b.len() && end - i < MAX_IDENT && is_ident_byte(b[end]) {
        end += 1;
    }
    while end > i && matches!(b[end - 1], b'.' | b'-') {
        end -= 1;
    }
    if !is_secret_name(&text[i..end]) {
        return None;
    }
    let flag = i >= 2 && b[i - 1] == b'-' && b[i - 2] == b'-';
    let mut j = end;
    // The closing quote of a quoted key (`"password"`), or its escape.
    let mut q = 0;
    while j < b.len() && q < 3 && matches!(b[j], b'"' | b'\'' | b'\\' | b'`') {
        j += 1;
        q += 1;
    }
    let mut spaced = false;
    while j < b.len() && matches!(b[j], b' ' | b'\t') {
        j += 1;
        spaced = true;
    }
    let mut sep: Option<u8> = None;
    if j < b.len() && matches!(b[j], b'=' | b':') {
        let c = b[j];
        let next = b.get(j + 1).copied();
        // `==` compares and `::` is a path: neither assigns. `${VAR:-x}`,
        // `${VAR:?msg}`, `${VAR:+x}` are shell expansions, and `:=` without a
        // space before it is `${VAR:=x}`.
        if (c == b'=' && next == Some(b'='))
            || (c == b':' && matches!(next, Some(b':') | Some(b'?') | Some(b'-') | Some(b'+')))
            || (c == b':' && next == Some(b'=') && !spaced)
        {
            return None;
        }
        sep = Some(c);
        j += 1;
        // `:=` (Go) and `=>` (Ruby, PHP) assign as well.
        if (c == b':' && b.get(j) == Some(&b'=')) || (c == b'=' && b.get(j) == Some(&b'>')) {
            j += 1;
        }
    }
    let mut v = j;
    let mut space_after = false;
    while v < b.len() && matches!(b[v], b' ' | b'\t') {
        v += 1;
        space_after = true;
    }
    match (sep, flag) {
        (None, false) => return None,
        // `--password value`: a flag followed by whitespace and its value.
        (None, true) if !spaced || b.get(v) == Some(&b'-') => return None,
        _ => {}
    }
    // A quote inside a JSON string is escaped: `\"value\"`.
    if v < b.len() && b[v] == b'\\' && matches!(b.get(v + 1), Some(b'"') | Some(b'\'')) {
        v += 1;
    }
    let quoted = v < b.len() && matches!(b[v], b'"' | b'\'');
    let (vs, ve) = if quoted {
        let q = b[v];
        let s = v + 1;
        let mut e = s;
        while e < b.len() && b[e] != q && !matches!(b[e], b'\\' | b'\n' | b'\r') {
            e += 1;
        }
        (s, e)
    } else {
        let mut e = v;
        while e < b.len() && !is_value_delim(b[e]) {
            e += 1;
        }
        (v, e)
    };
    if ve - vs < 3 || !text.is_char_boundary(vs) || !text.is_char_boundary(ve) {
        return None;
    }
    let value = &text[vs..ve];
    if is_placeholder(value)
        || is_reference(value)
        || value.contains(char::is_whitespace)
        || value.contains("://")
        // `initWithUser:password:persistence:` is a selector, not an assignment.
        || value.ends_with(':')
        || token_at(text, b, vs).is_some()
    {
        return None;
    }
    let env_style = sep == Some(b'=') && !spaced && !space_after;
    if !quoted {
        // A declaration, call, index, generic, path, attribute or macro is
        // code, and so is an unquoted value that a comma or semicolon ends.
        let after = b[ve..].iter().copied().find(|c| !matches!(c, b' ' | b'\t'));
        if matches!(b.get(ve), Some(b'(') | Some(b'[') | Some(b'<'))
            || value.contains("::")
            || dotted_identifier(value)
            || declared_in_code(text, i)
            || (value.ends_with('!') && matches!(after, Some(b'{') | Some(b'(') | Some(b'[')))
            || (!env_style && matches!(b.get(ve), Some(b',') | Some(b';')))
            || (!env_style && value.contains('*'))
        {
            return None;
        }
    }
    if !env_style {
        // `key: value`, `key = value`, `--flag value`: the value must look
        // like a credential, quoted or not — a quoted `"error"` after
        // `token:` is a parser's token, not a secret.
        let all_digits = value.bytes().all(|c| c.is_ascii_digit());
        if !secret_shaped(value) || (all_digits && !flag) {
            return None;
        }
    }
    Some(Hit {
        rule: "generic_assignment",
        start: vs,
        end: ve,
    })
}

/// True when `text` contains at least one secret.
pub fn contains_secret(text: &str) -> bool {
    let b = text.as_bytes();
    (0..b.len()).any(|i| at(text, b, i).is_some())
}

/// `text` with every secret span replaced by `[REDACTED:<rule>]`.
pub fn redact(text: &str) -> (String, usize) {
    match redact_str(text) {
        Some((out, hits)) => (out, hits.len()),
        None => (text.to_string(), 0),
    }
}

/// The redacted text and its hits, or `None` when nothing matched.
fn redact_str(text: &str) -> Option<(String, Vec<Hit>)> {
    let hits = scan(text);
    if hits.is_empty() {
        return None;
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
    Some((out, hits))
}

fn redact_string(s: &mut String, stats: &mut RedactionStats) -> usize {
    match redact_str(s) {
        Some((out, hits)) => {
            *s = out;
            stats.fields += 1;
            for h in &hits {
                stats.record(h.rule);
            }
            hits.len()
        }
        None => 0,
    }
}

/// A JSON member whose *name* says secret and whose string value is a
/// literal: the whole value goes. The string scanner cannot see the key, so
/// `{"password": "hunter2"}` needs this.
fn redact_member(key: &str, v: &mut Value, stats: &mut RedactionStats) -> usize {
    if let Value::String(s) = v {
        if is_secret_name(key) {
            let t = s.trim();
            if t.len() >= 3
                && !is_placeholder(t)
                && !is_reference(t)
                && !t.contains(char::is_whitespace)
                && !dotted_identifier(t)
            {
                *s = "[REDACTED:generic_assignment]".to_string();
                stats.fields += 1;
                stats.record("generic_assignment");
                return 1;
            }
        } else if key.to_ascii_lowercase().ends_with("authorization") {
            // `"authorization": "Bearer abc…"`: the header's value alone.
            let probe = format!("Authorization: {s}");
            if let Some((out, hits)) = redact_str(&probe) {
                *s = out["Authorization: ".len()..].to_string();
                stats.fields += 1;
                for h in &hits {
                    stats.record(h.rule);
                }
                return hits.len();
            }
        }
    }
    redact_json(v, stats)
}

fn redact_json(v: &mut Value, stats: &mut RedactionStats) -> usize {
    match v {
        Value::String(s) => redact_string(s, stats),
        Value::Array(items) => {
            let mut n = 0;
            for item in items {
                n += redact_json(item, stats);
            }
            n
        }
        Value::Object(map) => {
            let mut n = 0;
            for (k, val) in map.iter_mut() {
                n += redact_member(k, val, stats);
            }
            n
        }
        _ => 0,
    }
}

/// Redact every string inside a JSON value, in place. Returns the number of
/// spans redacted.
pub fn redact_value(v: &mut Value) -> usize {
    redact_json(v, &mut RedactionStats::default())
}

/// Replace every secret span in the content-bearing fields of one event —
/// `content` (prompt, command, message, error, tool input and output, extra)
/// and `raw` — with `[REDACTED:<rule>]`, and say what was done. Metadata
/// (`attrs`, `paths`, `project`) is untouched: `attrs` is held secret-free by
/// ingestion. A pure function of the event, so every path that moves content
/// somewhere new (upload, export, ingestion) can call it.
pub fn redact_event_content(ev: &mut Event) -> RedactionStats {
    let mut stats = RedactionStats::default();
    if let Some(c) = &mut ev.content {
        for s in [&mut c.prompt, &mut c.command, &mut c.message, &mut c.error]
            .into_iter()
            .flatten()
        {
            redact_string(s, &mut stats);
        }
        for v in [&mut c.tool_input, &mut c.tool_output]
            .into_iter()
            .flatten()
        {
            redact_json(v, &mut stats);
        }
        for (k, v) in c.extra.iter_mut() {
            redact_member(k, v, &mut stats);
        }
    }
    if let Some(raw) = &mut ev.raw {
        redact_json(raw, &mut stats);
    }
    stats
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

    #[test]
    fn prose_that_is_not_ascii_scans_without_panicking() {
        // The scan indexes bytes; Korean, emoji and accents put continuation
        // bytes where a rule would otherwise be tried.
        let text = "브랜치 정리하고 배포해줘 — clé: ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdef0123 끝 🚀";
        let hits = scan(text);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].rule, "github_token");
        let (r, n) = redact(text);
        assert_eq!(n, 1);
        assert!(r.starts_with("브랜치 정리하고 배포해줘"), "{r}");
        assert!(r.ends_with("끝 🚀"), "{r}");
        assert!(r.contains("[REDACTED:github_token]"), "{r}");
        assert!(!contains_secret("한글만 있는 프롬프트 🚀 — no secret here"));
    }

    // -- structural rules ---------------------------------------------------

    fn redacted(text: &str) -> String {
        redact(text).0
    }

    fn rules(text: &str) -> Vec<&'static str> {
        scan(text).into_iter().map(|h| h.rule).collect()
    }

    #[test]
    fn assignments_of_secret_named_keys_are_redacted() {
        let cases = [
            // `.env` and shell: no spaces around `=`, any non-placeholder value.
            (
                "DB_PASSWORD=hunter2",
                "DB_PASSWORD=[REDACTED:generic_assignment]",
            ),
            (
                "POSTGRES_PASSWORD=postgres\nPORT=5432",
                "POSTGRES_PASSWORD=[REDACTED:generic_assignment]\nPORT=5432",
            ),
            (
                "export SECRET_KEY=changeit && ./run",
                "export SECRET_KEY=[REDACTED:generic_assignment] && ./run",
            ),
            (
                "PASSWORD=\"hunter\" ./login",
                "PASSWORD=\"[REDACTED:generic_assignment]\" ./login",
            ),
            (
                "GET /cb?access_token=abc123def&state=1",
                "GET /cb?access_token=[REDACTED:generic_assignment]&state=1",
            ),
            // JSON, quoted, with and without spaces.
            (
                r#"{"password": "hunter2", "user": "bob"}"#,
                r#"{"password": "[REDACTED:generic_assignment]", "user": "bob"}"#,
            ),
            (
                r#"{"apiKey":"abcd1234","n":1}"#,
                r#"{"apiKey":"[REDACTED:generic_assignment]","n":1}"#,
            ),
            // JSON that was itself put in a string.
            (
                r#"{\"client_secret\": \"abcd1234\"}"#,
                r#"{\"client_secret\": \"[REDACTED:generic_assignment]\"}"#,
            ),
            // Code and config: a quoted literal, or a credential-shaped value.
            (
                "password = \"correct horse\"x",
                "password = \"correct horse\"x", // whitespace inside: prose, not a literal
            ),
            (
                "const clientSecret = 'abcd1234';",
                "const clientSecret = '[REDACTED:generic_assignment]';",
            ),
            (
                "password: s3cr3t!",
                "password: [REDACTED:generic_assignment]",
            ),
            (
                "X-Api-Key: abc123def456",
                "X-Api-Key: [REDACTED:generic_assignment]",
            ),
            // Flags.
            (
                "psql --password hunter2 -h db",
                "psql --password [REDACTED:generic_assignment] -h db",
            ),
            (
                "tool --api-key=abc123def456 run",
                "tool --api-key=[REDACTED:generic_assignment] run",
            ),
            (
                "tool --token 9f8e7d6c5b",
                "tool --token [REDACTED:generic_assignment]",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(redacted(input), expected, "{input}");
        }
    }

    #[test]
    fn the_specific_rule_names_a_token_even_when_a_name_introduces_it() {
        for (input, rule) in [
            (
                "GITHUB_TOKEN=ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdef0123",
                "github_token",
            ),
            (
                "{\"token\": \"eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U\"}",
                "jwt",
            ),
            (
                "api_key = 'sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789'",
                "anthropic_api_key",
            ),
            ("secret=AKIAIOSFODNN7EXAMPLE", "aws_access_key_id"),
        ] {
            assert_eq!(rules(input), [rule], "{input}");
        }
    }

    #[test]
    fn json_members_with_secret_names_lose_their_value() {
        let mut v = serde_json::json!({
            "command": "deploy",
            "password": "hunter2",
            "nested": { "api_key": "abcdefgh", "token": "string", "secret": "" },
            "list": [{ "authorization": "Bearer abc123def456ghi789" }],
            "max_tokens": 4096,
            "token_count": "12345"
        });
        let n = redact_value(&mut v);
        assert_eq!(n, 3, "{v}");
        assert_eq!(v["password"], "[REDACTED:generic_assignment]");
        assert_eq!(v["nested"]["api_key"], "[REDACTED:generic_assignment]");
        assert_eq!(v["nested"]["token"], "string", "a type name is no value");
        assert_eq!(v["nested"]["secret"], "");
        assert_eq!(
            v["list"][0]["authorization"],
            "Bearer [REDACTED:authorization_header]"
        );
        assert_eq!(v["max_tokens"], 4096);
        assert_eq!(v["token_count"], "12345");
        assert_eq!(v["command"], "deploy");
    }

    #[test]
    fn credentials_in_a_url_are_redacted_and_the_host_stays() {
        for (input, expected) in [
            (
                "postgres://app:hunter2@db.internal:5432/app",
                "postgres://[REDACTED:url_credentials]@db.internal:5432/app",
            ),
            (
                "REDIS_URL=redis://:pw0rd@cache:6379/0",
                "REDIS_URL=redis://[REDACTED:url_credentials]@cache:6379/0",
            ),
            (
                "git clone https://oauth2:abc123@gitlab.com/acme/repo.git",
                "git clone https://[REDACTED:url_credentials]@gitlab.com/acme/repo.git",
            ),
            (
                "amqps://u:pa%40ss@mq.example.com/vhost, next",
                "amqps://[REDACTED:url_credentials]@mq.example.com/vhost, next",
            ),
        ] {
            assert_eq!(redacted(input), expected, "{input}");
        }
        for benign in [
            "https://github.com/acme/repo.git",
            "ssh://git@github.com/acme/repo.git",
            "git@github.com:acme/repo.git",
            "http://localhost:8080/path@x:y",
            "https://example.com:443/a",
            "postgres://user:${DB_PASSWORD}@host/db",
            "postgres://user:$PASSWORD@host/db",
            "https://user:@host/",
            "mailto:me@example.com",
            "see http://host:3000?email=a@b.com",
            "postgres://user:****@host/db",
        ] {
            assert!(!contains_secret(benign), "{benign}");
        }
    }

    #[test]
    fn authorization_headers_lose_their_credential_only() {
        for (input, expected) in [
            (
                "Authorization: Bearer abc123def456ghi789",
                "Authorization: Bearer [REDACTED:authorization_header]",
            ),
            (
                "curl -H \"authorization: bearer abc123def456ghi789\" https://x",
                "curl -H \"authorization: bearer [REDACTED:authorization_header]\" https://x",
            ),
            (
                r#"{"Authorization": "Basic dXNlcjpwYXNzd29yZA=="}"#,
                r#"{"Authorization": "Basic [REDACTED:authorization_header]"}"#,
            ),
            // A long unbroken credential needs no digit.
            (
                "Authorization: Bearer abcdefghijklmnopqrstuvwxyzabcdef",
                "Authorization: Bearer [REDACTED:authorization_header]",
            ),
        ] {
            assert_eq!(redacted(input), expected, "{input}");
        }
        for benign in [
            "Authorization: Bearer $TOKEN",
            "Authorization: Bearer <token>",
            "Authorization: Bearer ${{ secrets.TOKEN }}",
            "the Authorization header: Bearer authentication is used",
            "Authorization: Bearer token",
            "Authorization required",
            "bearer abc123def456ghi789",
        ] {
            assert!(!contains_secret(benign), "{benign}");
        }
    }

    #[test]
    fn legacy_openai_keys_and_aws_secret_keys_next_to_their_label() {
        let legacy = format!("sk-{}", "a1B2c3D4".repeat(6));
        assert_eq!(rules(&format!("key {legacy} end")), ["openai_api_key"]);
        assert_eq!(
            redacted(&format!("OPENAI_API_KEY={legacy}")),
            "OPENAI_API_KEY=[REDACTED:openai_api_key]"
        );
        let secret = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        assert_eq!(secret.len(), 40);
        for input in [
            format!("aws_secret_access_key = {secret}"),
            format!("AWS_SECRET_ACCESS_KEY={secret}"),
            format!("\"SecretAccessKey\": \"{secret}\""),
        ] {
            let (r, n) = redact(&input);
            assert_eq!(n, 1, "{input}");
            assert!(r.contains("[REDACTED:aws_secret_key]"), "{r}");
            assert!(!r.contains("wJalrXUtn"), "{r}");
        }
        // The same 40 characters with no label are just 40 characters.
        assert!(!contains_secret(&format!("checksum {secret}")));
        // A label with a value that is not 40 characters is not an AWS key.
        assert!(!contains_secret("aws_secret_access_key = changeme"));
        assert!(!contains_secret("aws_secret_access_key=$AWS_SECRET"));
    }

    /// Text that is NOT a secret and must come through byte for byte. Every
    /// structural rule is held to this corpus: precision first.
    const FALSE_POSITIVE_CORPUS: &[&str] = &[
        // Hashes, ids, encodings.
        "commit 3f786850e387550fdab836ed7e6dc881de23001b",
        "sha256:2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae",
        "550e8400-e29b-41d4-a716-446655440000",
        "ev_0192a7c4-2b3f-7a11-9c2b-1f2e3d4c5b6a",
        "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==",
        "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        "sk-learn-pipeline-with-a-very-long-kebab-case-name-1234567890",
        "task-queue-worker-12345678901234567890123456789012",
        // Prose that talks about secrets.
        "The password reset flow emails a one-time token to the user.",
        "Enter your password: then press return",
        "Please rotate the secret and update the token in the vault.",
        "Set the password in the settings page; the token expires daily.",
        "Authorization: Bearer authentication is described in RFC 6750",
        "passwordless login uses a magic link",
        "Reset password: click the link we sent you",
        // Code that handles secrets without holding one.
        "let token = next_token();",
        "let password = user_input;",
        "password = user_password",
        "self.token = token",
        "api_key = os.getenv(\"API_KEY\")",
        "secret = secrets.token_hex(32)",
        "token = request.headers[\"x-token\"]",
        "const apiKey = settings.apiKey;",
        "pub token: Option<String>,",
        "password: String,",
        "token: string;",
        "password: str = Field(...)",
        "if token == other_token { return }",
        "std::env::var(\"API_KEY\")",
        "process.env.API_KEY",
        "TOKEN: ${{ secrets.GITHUB_TOKEN }}",
        "PASSWORD=$(cat /run/secrets/pw)",
        "export TOKEN=${TOKEN}",
        "password=<your-password>",
        "password: ********",
        "api_key: null",
        "\"password\": \"\"",
        "\"token\": \"string\"",
        "\"secret\": \"a description of what the secret is for\"",
        "password: null",
        // Names that merely contain the word.
        "token_count=5",
        "max_tokens: 4096",
        "input_tokens=1200 output_tokens=340",
        "tokenizer = load()",
        "secret_name: db-credentials",
        "password_field: pw",
        "password_hash: abc123",
        "SECRET_KEY_BASE=",
        "--password-stdin",
        "--password-file /run/secrets/pw",
        "--token-file ~/.config/tool/token",
        "--token to authenticate",
        "pass: 1",
        "PWD=/home/dev/project",
        "key: value",
        "key=abc123def456",
        "monkey=banana1234",
        // Code from real crates (a registry sweep found each of these).
        "pub password: *const c_char,",
        "password: *mut *mut c_char,",
        "colon2_token: Colon2,",
        "dot2_token: node.dot2_token,",
        "inner: ErrorKind::Unexpected { token: \"error\" },",
        "assert_eq!(\"unexpected token: \\\"error\\\"\", e.to_string());",
        "Self::AccessKey => \"azure_storage_account_key\",",
        "ClientEarlyTrafficSecret => \"CLIENT_EARLY_TRAFFIC_SECRET\",",
        "Password = \"password\",",
        "let geometry_token = display2",
        "let token = match_byte! { b,",
        "token          = 1*tchar",
        "password: \"pass\",",
        "publicKeyToken=\"6595b64144ccf1df\"",
        "NEXT_PUBLIC_API_KEY=pk_abc123xyz",
        "-H \"Authorization: Bearer OAUTH2_TOKEN\" \\",
        "API_KEY=YOUR_API_KEY",
        "token: ${{ secrets.NPM_TOKEN }}",
        "#[unsafe(method(initWithUser:password:persistence:))]",
        "${VAR:-default} and ${TOKEN:?set TOKEN}",
        // Numbers where a value would be.
        "token: 12345",
        "secret: 0",
        // URLs and paths.
        "https://github.com/nullarch/attemptdb/issues/12",
        "/Users/dev/project/src/auth/password.rs",
        "src/lib.rs:42",
    ];

    #[test]
    fn the_false_positive_corpus_stays_unredacted() {
        for text in FALSE_POSITIVE_CORPUS {
            let hits = scan(text);
            assert!(hits.is_empty(), "{text:?} was matched: {hits:?}");
            assert!(!contains_secret(text), "{text}");
            assert_eq!(redact(text), (text.to_string(), 0), "{text}");
        }
        // Surrounded by more of the same, in one document.
        let doc = FALSE_POSITIVE_CORPUS.join("\n");
        assert_eq!(redact(&doc).1, 0);
    }

    #[test]
    fn structural_rules_scan_hostile_input_without_panicking_or_blowing_up() {
        let long_dots = "a.".repeat(50_000);
        let long_dash = "token-".repeat(20_000);
        let long_word = "password".repeat(10_000);
        let long_url = format!("http://{}", "u:".repeat(10_000));
        let start = std::time::Instant::now();
        for text in [&long_dots, &long_dash, &long_word, &long_url] {
            let _ = scan(text);
        }
        assert!(
            start.elapsed() < std::time::Duration::from_secs(10),
            "scan time grew with the square of the input"
        );
        // Non-ASCII around every rule.
        assert_eq!(
            redacted("비밀번호 password=한글비밀번호 끝 — token: 알수없음1 🚀"),
            "비밀번호 password=[REDACTED:generic_assignment] 끝 — token: [REDACTED:generic_assignment] 🚀"
        );
    }

    #[test]
    fn an_events_content_is_redacted_in_every_field_and_the_stats_say_how_much() {
        use crate::event::{EventContent, Provider};
        use crate::{CaptureMode, DeviceId, EventKind, ProjectRef};
        let device = DeviceId::derive(&["secrets-test"]);
        let mut ev = Event::new(
            device,
            Provider::ClaudeCode,
            "PostToolUse",
            EventKind::ToolCallFinished,
            ProjectRef::derive("/home/dev/example", None, &device),
            "s1",
            CaptureMode::LocalSemantic,
            "test",
        );
        ev.attrs
            .insert("x_test_note".into(), serde_json::json!("keep"));
        ev.content = Some(EventContent {
            prompt: Some("deploy with DB_PASSWORD=hunter2 please".into()),
            command: Some("psql postgres://app:hunter2@db/app".into()),
            message: Some("the password reset flow".into()),
            error: Some("401 for Authorization: Bearer abc123def456ghi789".into()),
            tool_input: Some(serde_json::json!({"password": "hunter2", "path": "a.txt"})),
            tool_output: Some(serde_json::json!(
                "AWS_SECRET_ACCESS_KEY=wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
            )),
            ..Default::default()
        });
        ev.raw = Some(
            serde_json::json!({"env": {"GITHUB_TOKEN": "ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZabcdef0123"}}),
        );
        let stats = redact_event_content(&mut ev);
        let text = serde_json::to_string(&ev).unwrap();
        for leaked in ["hunter2", "abc123def456", "wJalrXUtnFEMI", "ghp_ABCDEF"] {
            assert!(!text.contains(leaked), "{leaked} survived: {text}");
        }
        assert!(text.contains("the password reset flow"), "prose untouched");
        assert!(text.contains("a.txt"), "other members untouched");
        assert_eq!(ev.attrs["x_test_note"], "keep", "attrs are not content");
        assert_eq!(stats.spans, 6, "{stats:?}");
        assert!(stats.fields >= 6);
        assert_eq!(stats.by_rule["url_credentials"], 1);
        assert_eq!(stats.by_rule["authorization_header"], 1);
        assert_eq!(stats.by_rule["aws_secret_key"], 1);
        // A second pass finds nothing: redaction is idempotent.
        assert!(redact_event_content(&mut ev).is_empty());
        // An event with no content costs nothing.
        let mut bare = ev.clone();
        bare.content = None;
        bare.raw = None;
        assert!(redact_event_content(&mut bare).is_empty());
    }

    #[test]
    fn attrs_values_with_credentials_in_urls_or_headers_are_dropped_by_the_value_check() {
        assert!(!crate::attrs::value_allowed("postgres://u:p4ss@db/app"));
        assert!(!crate::attrs::value_allowed("DB_PASSWORD=hunter2"));
        assert!(crate::attrs::value_allowed("token_count=5"));
        assert!(crate::attrs::value_allowed("https://github.com/acme/repo"));
    }
}
