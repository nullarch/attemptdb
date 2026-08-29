//! Readable prefixed ids (`ev_…`, `ses_…`, `att_…`) and short-id resolution.
//!
//! Core id types display as bare UUIDs; the query layer always renders them
//! with their type prefix so a value in any result row can be pasted back
//! into a statement unambiguously.

use crate::error::{QueryError, Result};
use attemptdb_core::{
    AgentId, AttemptId, DecisionId, DeviceId, EventId, ProjectId, SessionId, SpanId, TurnId,
    WorkUnitId,
};
use std::fmt;
use std::str::FromStr;

/// An id type with a display prefix.
pub trait PrefixedId: Copy + fmt::Display + FromStr {
    const PREFIX: &'static str;

    /// 32 lowercase hex characters, no hyphens.
    fn hex(&self) -> String;

    /// `prefix + hyphenated uuid`, e.g. `ses_0191c2a3-…`.
    fn readable(&self) -> String {
        format!("{}{}", Self::PREFIX, self)
    }
}

macro_rules! prefixed {
    ($($t:ty),* $(,)?) => {
        $(
            impl PrefixedId for $t {
                const PREFIX: &'static str = <$t>::PREFIX;
                fn hex(&self) -> String {
                    self.0.simple().to_string()
                }
            }
        )*
    };
}

prefixed!(
    EventId, SessionId, ProjectId, AgentId, AttemptId, TurnId, SpanId, DeviceId, WorkUnitId,
    DecisionId
);

pub fn readable<T: PrefixedId>(id: &T) -> String {
    id.readable()
}

pub fn readable_opt<T: PrefixedId>(id: &Option<T>) -> Option<String> {
    id.as_ref().map(readable)
}

pub fn readable_list<T: PrefixedId>(ids: &[T]) -> Vec<String> {
    ids.iter().map(readable).collect()
}

/// Every display prefix the core id types define.
pub const KNOWN_PREFIXES: &[&str] = &[
    "ev_", "dev_", "prj_", "ses_", "trn_", "spn_", "agt_", "att_", "wu_", "dec_", "art_", "inf_",
    "cor_",
];

/// Display prefix for a 16-byte binary id column, by column name. Unknown
/// columns render as bare UUIDs.
pub fn prefix_for_column(name: &str) -> &'static str {
    match name {
        "event_id" | "first_event_id" | "last_event_id" | "start_event_id" | "end_event_id"
        | "prompt_event_id" | "stop_event_id" | "cleared_by" => "ev_",
        "device_id" => "dev_",
        "project_id" => "prj_",
        "session_id" | "from_session" | "to_session" => "ses_",
        "span_id" | "parent_span_id" | "tool_call_id" => "spn_",
        "agent_id" | "parent_agent_id" => "agt_",
        "attempt_id" | "superseded_by" | "supersedes" | "last_attempt" => "att_",
        "turn_id" | "current_turn" => "trn_",
        "work_unit_id" => "wu_",
        "decision_id" => "dec_",
        _ => "",
    }
}

/// Hyphenated UUID text for 16 raw bytes.
pub fn hyphenated(bytes: [u8; 16]) -> String {
    EventId::from_bytes(bytes).to_string()
}

/// Split `ses_abc…` into `(Some("ses_"), "abc…")`; text without a known
/// prefix comes back unchanged.
pub fn split_prefix(text: &str) -> (Option<&str>, &str) {
    if let Some(i) = text.find('_') {
        let prefix = &text[..=i];
        if KNOWN_PREFIXES.contains(&prefix) {
            return (Some(prefix), &text[i + 1..]);
        }
    }
    (None, text)
}

/// Whether the text is shaped like an id: an optional known prefix followed
/// by at least four hex digits (hyphens allowed).
pub fn looks_like_id(text: &str) -> bool {
    let (_, rest) = split_prefix(text.trim());
    let hex: String = rest.chars().filter(|c| *c != '-').collect();
    hex.len() >= 4 && hex.chars().all(|c| c.is_ascii_hexdigit())
}

/// Resolve textual id input to a typed id.
///
/// Accepts the full UUID with or without the type prefix, or a short prefix
/// of at least four hex digits that matches exactly one candidate. A full
/// UUID is only checked against `candidates` when `must_exist` is set.
pub fn resolve<T: PrefixedId>(
    text: &str,
    candidates: &[T],
    what: &str,
    must_exist: bool,
) -> Result<T> {
    let t = text.trim();
    let (prefix, rest) = split_prefix(t);
    if let Some(p) = prefix
        && p != T::PREFIX
    {
        return Err(QueryError::not_found(format!(
            "expected {what} id ({}…), got '{t}'",
            T::PREFIX
        )));
    }
    let compact: String = rest
        .chars()
        .filter(|c| *c != '-')
        .collect::<String>()
        .to_ascii_lowercase();
    if compact.is_empty() || !compact.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(QueryError::plan(format!(
            "invalid {what} id '{t}' (expected {}<uuid> or a short hex prefix)",
            T::PREFIX
        )));
    }
    if compact.len() == 32 {
        let id = compact
            .parse::<T>()
            .map_err(|_| QueryError::plan(format!("invalid {what} id '{t}'")))?;
        if must_exist && !candidates.iter().any(|c| c.hex() == compact) {
            return Err(QueryError::not_found(format!(
                "{what} {} is not in the loaded events",
                id.readable()
            )));
        }
        return Ok(id);
    }
    if compact.len() < 4 {
        return Err(QueryError::plan(format!(
            "short {what} id '{t}' needs at least 4 hex digits"
        )));
    }
    let matches: Vec<&T> = candidates
        .iter()
        .filter(|c| c.hex().starts_with(&compact))
        .collect();
    match matches.len() {
        0 => Err(QueryError::not_found(format!("no {what} matches '{t}'"))),
        1 => Ok(*matches[0]),
        n => {
            let mut shown: Vec<String> = matches.iter().take(5).map(|c| c.readable()).collect();
            if n > 5 {
                shown.push(format!("… {} more", n - 5));
            }
            Err(QueryError::not_found(format!(
                "ambiguous {what} id '{t}': matches {}",
                shown.join(", ")
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_full_prefixed_and_short_ids() {
        let a = SessionId::derive(&["a"]);
        let b = SessionId::derive(&["b"]);
        let all = vec![a, b];
        assert_eq!(
            resolve::<SessionId>(&a.readable(), &all, "session", true).unwrap(),
            a
        );
        assert_eq!(
            resolve::<SessionId>(&a.to_string(), &all, "session", true).unwrap(),
            a
        );
        let short = format!("ses_{}", &a.hex()[..8]);
        assert_eq!(
            resolve::<SessionId>(&short, &all, "session", true).unwrap(),
            a
        );
        assert!(matches!(
            resolve::<SessionId>("ses_", &all, "session", true),
            Err(QueryError::Plan(_))
        ));
        assert!(matches!(
            resolve::<SessionId>(&AttemptId::derive(&["x"]).readable(), &all, "session", true),
            Err(QueryError::NotFound(_))
        ));
        assert!(matches!(
            resolve::<SessionId>("ses_zzzz", &all, "session", true),
            Err(QueryError::Plan(_))
        ));
    }

    #[test]
    fn readable_uses_prefix() {
        let id = EventId::derive(&["e"]);
        assert!(readable(&id).starts_with("ev_"));
        assert_eq!(readable(&id).len(), 3 + 36);
        assert!(looks_like_id("att_01a04762"));
        assert!(!looks_like_id("attempts"));
    }
}
