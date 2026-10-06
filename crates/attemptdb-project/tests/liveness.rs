//! Honest state: a session is `open`, `stale` or `closed` as of an explicit
//! instant, its confidence says how much of it was observed, and a wait for
//! a human is only ever ended by the agent that was waiting.

mod common;

use attemptdb_core::{AgentId, Event, Outcome, Timestamp};
use attemptdb_project::{
    AttentionKind, CoverageGrade, DEFAULT_MIN_CONFIDENCE, EdgeKind, Phase, Projection, Projector,
    SessionStatus, ToolPairing, WorkUnitStatus, project,
};
use common::{Sess, Stream, Tool, at};

const MIN: i64 = 60;
const HOUR: i64 = 3_600;
const DAY: i64 = 86_400;

/// Judge `events` at `now`, as a live reader does with the wall clock.
fn judged_at(events: &[Event], now: Timestamp) -> Projection {
    let mut p = Projector::new();
    for ev in events {
        p.push(ev);
    }
    p.finish_at(now)
}

fn killed_session(id: &str) -> (Sess, Vec<Event>) {
    let s = Sess::claude(id);
    let mut b = Stream::new();
    b.session_started(&s, at(0));
    b.prompt(&s, at(5), "fix it");
    b.tool_start(&s, at(10), &Tool::edit(Some("a"), &["src/a.rs"]));
    b.tool_finish(
        &s,
        at(11),
        &Tool::edit(Some("a"), &["src/a.rs"]),
        Outcome::success(),
    );
    b.stop(&s, at(20));
    (s, b.build())
}

#[test]
fn a_session_with_no_end_goes_stale_when_the_clock_says_so() {
    let (s, events) = killed_session("killed");

    // Judged against the stream itself, nothing has aged.
    let p = project(&events);
    assert_eq!(p.sessions[0].state, SessionStatus::Open);

    // Ten minutes after its last event: still plausibly alive.
    let p = judged_at(&events, at(20 + 10 * MIN));
    assert_eq!(p.session(s.session_id).unwrap().state, SessionStatus::Open);
    assert_eq!(p.reference_time, at(20 + 10 * MIN));

    // Fourteen days later it is stale, never "open".
    let p = judged_at(&events, at(14 * DAY));
    let session = p.session(s.session_id).unwrap();
    assert_eq!(session.state, SessionStatus::Stale);
    assert_eq!(session.ended_at, None, "staleness is not an end event");

    // The boundary is the RFC 0003 §5.1 thirty minutes, exclusive.
    let edge = at(20 + 30 * MIN);
    assert_eq!(
        judged_at(&events, edge).sessions[0].state,
        SessionStatus::Open
    );
    assert_eq!(
        judged_at(&events, at(20 + 30 * MIN + 1)).sessions[0].state,
        SessionStatus::Stale
    );
}

#[test]
fn an_ended_session_is_closed_whatever_the_clock_says() {
    let (s, mut events) = killed_session("ended");
    let mut b = Stream::new();
    b.events = events.clone();
    b.session_ended(&s, at(30), "exit");
    events = b.build();
    let p = judged_at(&events, at(14 * DAY));
    assert_eq!(p.sessions[0].state, SessionStatus::Closed);
}

#[test]
fn session_confidence_reflects_what_was_observed() {
    // Full lifecycle: start, prompt, tool calls, end.
    let s = Sess::claude("full");
    let mut b = Stream::new();
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "x");
    b.tool_start(&s, at(2), &Tool::shell(Some("c")));
    b.tool_finish(&s, at(3), &Tool::shell(Some("c")), Outcome::success());
    b.stop(&s, at(4));
    b.session_ended(&s, at(5), "exit");
    let p = judged_at(&b.build(), at(14 * DAY));
    assert_eq!(p.sessions[0].coverage, CoverageGrade::Full);
    assert_eq!(p.sessions[0].confidence(), 1.0, "start and end observed");

    // A live session: no end observed yet.
    let (_, events) = killed_session("live");
    let p = judged_at(&events, at(60));
    assert_eq!(p.sessions[0].coverage, CoverageGrade::Partial);
    assert_eq!(p.sessions[0].confidence(), 0.8);

    // The same session once it has gone silent: that it is over is a guess.
    let p = judged_at(&events, at(14 * DAY));
    assert_eq!(p.sessions[0].state, SessionStatus::Stale);
    assert_eq!(p.sessions[0].confidence(), 0.7);

    // Activity only: neither start nor end was seen.
    let m = Sess::claude("minimal");
    let mut b = Stream::new();
    b.prompt(&m, at(1), "x");
    b.tool_start(&m, at(2), &Tool::shell(Some("c")));
    b.tool_finish(&m, at(3), &Tool::shell(Some("c")), Outcome::success());
    let p = project(&b.build());
    assert_eq!(p.sessions[0].coverage, CoverageGrade::Minimal);
    assert_eq!(p.sessions[0].confidence(), 0.6);

    // Nothing a turn can be built from.
    let u = Sess::claude("nothing");
    let mut b = Stream::new();
    b.notification(&u, at(1), "teammate_idle");
    let p = project(&b.build());
    assert_eq!(p.sessions[0].coverage, CoverageGrade::Unknown);
    assert_eq!(p.sessions[0].confidence(), 0.4);
}

#[test]
fn a_session_waiting_on_a_human_stays_open_for_longer_than_a_silent_one() {
    let s = Sess::claude("waiting");
    let mut b = Stream::new();
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "do it");
    b.permission_requested(&s, at(2), &Tool::shell(Some("c1")));
    let events = b.build();

    // Nothing happens in a session that waits for an approval: three hours
    // of silence is the expected state, not evidence that it died.
    let p = judged_at(&events, at(3 * HOUR));
    assert_eq!(p.sessions[0].state, SessionStatus::Open);
    assert_eq!(
        p.attention_at(at(3 * HOUR), DEFAULT_MIN_CONFIDENCE).len(),
        1,
        "the approval is still waiting for a person"
    );

    // Two days later nobody is waiting any more.
    let p = judged_at(&events, at(2 * DAY));
    assert_eq!(p.sessions[0].state, SessionStatus::Stale);
    assert!(
        p.attention_at(at(2 * DAY), DEFAULT_MIN_CONFIDENCE)
            .is_empty()
    );
}

#[test]
fn a_session_used_again_after_its_end_is_resumed_and_reaches_needs_you() {
    let s = Sess::claude("resumed");
    let mut b = Stream::new();
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "first");
    b.tool_start(&s, at(10), &Tool::edit(Some("a"), &["src/a.rs"]));
    b.tool_finish(
        &s,
        at(11),
        &Tool::edit(Some("a"), &["src/a.rs"]),
        Outcome::success(),
    );
    b.stop(&s, at(20));
    b.session_ended(&s, at(30), "other");
    // A late completion trailing the end does not reopen anything.
    b.agent_message(&s, at(31));
    let ended = project(&b.events);
    assert_eq!(ended.sessions[0].state, SessionStatus::Closed);
    assert!(ended.sessions[0].ended_at.is_some());

    // The same id comes back an hour later.
    b.session_started(&s, at(HOUR));
    b.prompt(&s, at(HOUR + 1), "second");
    b.tool_start(&s, at(HOUR + 10), &Tool::edit(Some("b"), &["src/b.rs"]));
    b.permission_requested(&s, at(HOUR + 11), &Tool::edit(Some("b"), &["src/b.rs"]));
    let p = project(&b.events);
    let session = &p.sessions[0];
    assert_eq!(session.ended_at, None, "it has not ended any more");
    assert_eq!(session.end_event_id, None);
    assert_eq!(session.state, SessionStatus::Open);
    assert_eq!(session.coverage, CoverageGrade::Partial);
    assert_eq!(p.state_at(at(HOUR + 15)).sessions.len(), 1);
    let queue = p.attention_at(at(HOUR + 15), DEFAULT_MIN_CONFIDENCE);
    assert_eq!(queue.len(), 1, "the resumed session's approval is found");
    assert_eq!(queue[0].kind, AttentionKind::PermissionGate);
}

#[test]
fn idle_prompt_after_a_completed_turn_is_not_needs_you() {
    let s = Sess::claude("idle");
    let mut b = Stream::new();
    b.session_started(&s, at(0));
    b.prompt(&s, at(5), "fix it");
    b.tool_start(&s, at(10), &Tool::edit(Some("a"), &["src/a.rs"]));
    b.tool_finish(
        &s,
        at(11),
        &Tool::edit(Some("a"), &["src/a.rs"]),
        Outcome::success(),
    );
    b.stop(&s, at(20));
    b.notification(&s, at(80), "idle_prompt");
    let events = b.build();

    for now in [at(90), at(14 * DAY)] {
        let p = judged_at(&events, now);
        assert_eq!(p.signals.len(), 1, "the notification was observed");
        assert!(!p.signals[0].blocking);
        assert!(p.attention_at(now, DEFAULT_MIN_CONFIDENCE).is_empty());
        assert!(p.why_blocked(s.session_id).is_none());
        assert!(!p.state_at(now).sessions.iter().any(|st| st.blocked));
        let u = &p.work_units[0];
        assert_ne!(u.phase, Phase::Blocked, "{}", u.phase_reason);
        assert_eq!(u.blocking_signal, None);
    }

    // The same notification while a turn is still running is the agent
    // asking for something.
    let mut b = Stream::new();
    b.session_started(&s, at(0));
    b.prompt(&s, at(5), "fix it");
    b.tool_start(&s, at(10), &Tool::read(Some("a"), &["src/a.rs"]));
    b.tool_finish(
        &s,
        at(11),
        &Tool::read(Some("a"), &["src/a.rs"]),
        Outcome::success(),
    );
    b.notification(&s, at(80), "idle_prompt");
    let p = project(&b.build());
    assert!(p.signals[0].blocking);
    let queue = p.attention_at(at(90), DEFAULT_MIN_CONFIDENCE);
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].kind, AttentionKind::InputRequest);
}

#[test]
fn a_permission_wait_is_cleared_only_by_the_agent_that_was_waiting() {
    let s = Sess::claude("agents");
    let sub = AgentId::derive(&["subagent"]);
    let mut b = Stream::new();
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "do stuff");
    b.tool_start(&s, at(10), &Tool::edit(Some("main1"), &["src/a.rs"]));
    let sig = b.permission_requested(&s, at(11), &Tool::edit(Some("main1"), &["src/a.rs"]));
    // A background subagent keeps working while the main agent waits.
    let t2 = b.tool_start(&s, at(15), &Tool::read(Some("sub1"), &["src/b.rs"]));
    let t3 = b.tool_finish(
        &s,
        at(16),
        &Tool::read(Some("sub1"), &["src/b.rs"]),
        Outcome::success(),
    );
    let mut events = b.build();
    for e in events.iter_mut() {
        if e.event_id == t2 || e.event_id == t3 {
            e.agent.agent_id = sub;
        }
    }
    let p = project(&events);
    assert_eq!(p.signals[0].event_id, sig);
    assert_eq!(
        p.signals[0].cleared_at, None,
        "the subagent's progress is not the main agent's approval"
    );
    assert_eq!(p.work_units[0].phase, Phase::Blocked);
    assert_eq!(p.attention_at(at(100), DEFAULT_MIN_CONFIDENCE).len(), 1);

    // The waiting agent's own next call does clear it.
    let mut b = Stream::new();
    b.events = events.clone();
    let resumed = b.tool_finish(
        &s,
        at(30),
        &Tool::edit(Some("main1"), &["src/a.rs"]),
        Outcome::success(),
    );
    let p = project(&b.build());
    assert_eq!(p.signals[0].cleared_by, Some(resumed));

    // A human prompt answers everybody.
    let mut b = Stream::new();
    b.events = events;
    let prompt = b.prompt(&s, at(40), "yes, go on");
    let p = project(&b.build());
    assert_eq!(p.signals[0].cleared_by, Some(prompt));
}

#[test]
fn an_interrupted_turn_ages_instead_of_staying_in_progress_forever() {
    let s = Sess::claude("interrupted");
    let mut b = Stream::new();
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "big refactor");
    b.tool_start(&s, at(10), &Tool::edit(Some("e1"), &["src/a.rs"]));
    b.tool_finish(
        &s,
        at(11),
        &Tool::edit(Some("e1"), &["src/a.rs"]),
        Outcome::success(),
    );
    // The user hits Esc and closes the terminal: no Stop, no SessionEnd.
    let events = b.build();

    let soon = judged_at(&events, at(30 * MIN));
    let u = &soon.work_units[0];
    assert_eq!(u.status, WorkUnitStatus::Open, "{}", u.status_reason);
    assert!(u.status_reason.contains("in progress"));

    let late = at(5 * DAY);
    let p = judged_at(&events, late);
    let u = &p.work_units[0];
    assert_eq!(u.status, WorkUnitStatus::Abandoned, "{}", u.status_reason);
    assert!(u.status_reason.contains("never stopped"));
    assert_eq!(p.sessions[0].state, SessionStatus::Stale);
    // `STATE ... AT now` and the stored units are judged the same way.
    assert_eq!(p.work_units_at(late), p.work_units);
}

#[test]
fn a_session_that_waits_on_a_human_is_not_abandoned() {
    let s = Sess::claude("blocked-unit");
    let mut b = Stream::new();
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "do it");
    b.permission_requested(&s, at(2), &Tool::shell(Some("c1")));
    let events = b.build();
    let p = judged_at(&events, at(4 * HOUR));
    let u = &p.work_units[0];
    assert_eq!(u.status, WorkUnitStatus::Open, "{}", u.status_reason);
    assert_eq!(u.phase, Phase::Blocked);
    assert!(u.status_reason.contains("waiting on a human"));
}

#[test]
fn heuristic_edges_and_fifo_calls_do_not_claim_certainty() {
    // Failed edit, retried on the same path: `caused` and `superseded`.
    let s = Sess::claude("edges");
    let mut b = Stream::new();
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "fix");
    let fail_start = b.tool_start(&s, at(5), &Tool::edit(Some("e1"), &["src/a.rs"]));
    let fail_end = b.tool_failed(
        &s,
        at(6),
        &Tool::edit(Some("e1"), &["src/a.rs"]),
        "mismatch",
    );
    let retry_start = b.tool_start(&s, at(10), &Tool::edit(Some("e2"), &["src/a.rs"]));
    b.tool_finish(
        &s,
        at(11),
        &Tool::edit(Some("e2"), &["src/a.rs"]),
        Outcome::success(),
    );
    b.stop(&s, at(20));
    b.session_ended(&s, at(21), "exit");
    let _ = fail_start;
    let p = project(&b.build());
    let caused = p
        .edges
        .iter()
        .find(|e| {
            e.kind == EdgeKind::Caused
                && e.from == attemptdb_project::EdgeEndpoint::Event(fail_end)
                && e.to == attemptdb_project::EdgeEndpoint::Event(retry_start)
        })
        .expect("caused edge");
    assert!(
        caused.confidence <= attemptdb_project::HEURISTIC_EDGE_CONFIDENCE,
        "adjacency plus a shared path is a guess: {}",
        caused.confidence
    );
    let superseded = p
        .edges
        .iter()
        .find(|e| e.kind == EdgeKind::Superseded)
        .expect("superseded edge");
    assert_eq!(superseded.confidence, p.attempts[0].confidence);
    assert!(superseded.confidence < 1.0);

    // A signal-derived edge carries the signal's confidence in the graph
    // layer; here the projection's own edges are all 1.0 only when they are
    // structural.
    for e in &p.edges {
        match e.kind {
            EdgeKind::ParentOf | EdgeKind::EvidenceFor | EdgeKind::Triggered => {}
            _ => assert!(e.confidence < 1.0, "{:?} {}", e.kind, e.confidence),
        }
    }
}

#[test]
fn tool_call_confidence_follows_the_pairing() {
    let s = Sess::claude("pairing");
    let mut b = Stream::new();
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "go");
    // By call id.
    b.tool_start(&s, at(2), &Tool::read(Some("r1"), &["a.rs"]));
    b.tool_finish(
        &s,
        at(3),
        &Tool::read(Some("r1"), &["a.rs"]),
        Outcome::success(),
    );
    // First-in first-out: no call ids.
    b.tool_start(&s, at(4), &Tool::shell(None));
    b.tool_finish(&s, at(5), &Tool::shell(None), Outcome::success());
    // An end with no start.
    b.tool_finish(
        &s,
        at(6),
        &Tool::read(Some("lone"), &["b.rs"]),
        Outcome::success(),
    );
    // A start with no end.
    b.tool_start(&s, at(7), &Tool::read(Some("open"), &["c.rs"]));
    let p = project(&b.build());
    let calls: Vec<_> = p.tool_calls_of(s.session_id).collect();
    let by = |pairing: ToolPairing| {
        calls
            .iter()
            .find(|c| c.pairing == pairing)
            .unwrap_or_else(|| panic!("no {pairing:?} call"))
            .confidence()
    };
    assert_eq!(by(ToolPairing::CallId), 1.0);
    assert_eq!(by(ToolPairing::Fifo), 0.9, "RFC 0003 §5.3");
    assert_eq!(by(ToolPairing::EndOnly), 1.0);
    assert_eq!(by(ToolPairing::InFlight), 0.7);
}
