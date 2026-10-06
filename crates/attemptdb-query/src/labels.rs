//! How the surfaces name the end of something that has no recorded end.
//!
//! A session without a `SessionEnded` is not necessarily open: agents are
//! killed far more often than they exit, so one that has been silent past
//! the staleness threshold is `stale` (see
//! [`SessionStatus`]), an inference from silence. Every surface (CLI, MCP,
//! the web UI and its export) prints an unfinished session through
//! [`session_end`] so that none of them says "open" for one the projection
//! calls stale.

use attemptdb_core::Timestamp;
use attemptdb_project::{Session, SessionStatus};

/// The end of `session` as text: its end time (through `fmt_time`) when one
/// was observed, `stale` when it has been silent for too long to count as
/// open, `open` otherwise.
pub fn session_end(session: &Session, fmt_time: impl Fn(Timestamp) -> String) -> String {
    match session.ended_at {
        Some(end) => fmt_time(end),
        None => session_state_word(session.state).to_string(),
    }
}

/// `open`, `stale` or `closed`: the word for a session's state.
pub fn session_state_word(state: SessionStatus) -> &'static str {
    state.as_str()
}

/// What to print where a turn or an attempt has no end: `open` while its
/// session is open, `stale` when the session went silent, and `cut off` when
/// the session ended with this still unfinished. `None` (the session is not
/// loaded) reads as `unfinished`.
pub fn unfinished(session: Option<&Session>) -> &'static str {
    match session.map(|s| s.state) {
        Some(SessionStatus::Open) => "open",
        Some(SessionStatus::Stale) => "stale",
        Some(SessionStatus::Closed) => "cut off",
        None => "unfinished",
    }
}

/// A sentence fragment for a session that is not closed: what is known and
/// what is guessed. `None` for a closed session.
pub fn liveness_note(session: &Session, fmt_time: impl Fn(Timestamp) -> String) -> Option<String> {
    match session.state {
        SessionStatus::Open => Some("still open (no session end observed)".to_string()),
        SessionStatus::Stale => Some(format!(
            "stale (no session end observed and no activity since {}; inferred from silence, not observed)",
            fmt_time(session.last_activity_at)
        )),
        SessionStatus::Closed => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use attemptdb_core::Timestamp;

    fn ts(t: Timestamp) -> String {
        t.to_rfc3339()
    }

    #[test]
    fn a_stale_session_is_never_called_open() {
        let mut s = Session {
            session_id: attemptdb_core::SessionId::nil(),
            provider: attemptdb_core::event::Provider::ClaudeCode,
            provider_session_id: "x".into(),
            project_id: attemptdb_core::ProjectId::nil(),
            project_name: "p".into(),
            started_at: Timestamp::from_micros(0),
            ended_at: None,
            end_reason: None,
            start_source: None,
            event_count: 1,
            turn_count: 0,
            prompt_count: 0,
            tool_call_count: 0,
            failure_count: 0,
            agents: Vec::new(),
            coverage: attemptdb_project::CoverageGrade::Partial,
            first_event_id: attemptdb_core::EventId::nil(),
            last_event_id: attemptdb_core::EventId::nil(),
            last_event_at: Timestamp::from_micros(0),
            start_event_id: None,
            end_event_id: None,
            state: SessionStatus::Stale,
            last_activity_at: Timestamp::from_micros(0),
        };
        assert_eq!(session_end(&s, ts), "stale");
        assert_eq!(unfinished(Some(&s)), "stale");
        assert!(liveness_note(&s, ts).unwrap().starts_with("stale ("));
        s.state = SessionStatus::Open;
        assert_eq!(session_end(&s, ts), "open");
        assert_eq!(unfinished(Some(&s)), "open");
        s.state = SessionStatus::Closed;
        s.ended_at = Some(Timestamp::from_micros(1_000_000));
        assert_eq!(session_end(&s, ts), ts(Timestamp::from_micros(1_000_000)));
        assert_eq!(unfinished(Some(&s)), "cut off");
        assert!(liveness_note(&s, ts).is_none());
    }
}
