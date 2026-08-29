mod common;

use attemptdb_core::event::Provider;
use attemptdb_core::{CaptureMode, Event, Hlc, Outcome, OutcomeStatus, SessionId, Timestamp};
use attemptdb_project::{
    ALGORITHM_VERSION, Attempt, AttemptOutcome, CoverageGrade, EdgeEndpoint, EdgeKind, Projection,
    Projector, Session, Turn, TurnStatus, project,
};
use common::{Sess, Stream, Tool, at, spec_scenario, spec_scenario_with};

fn session<'a>(p: &'a Projection, s: &Sess) -> &'a Session {
    p.session(s.session_id).expect("session projected")
}

fn turns<'a>(p: &'a Projection, s: &Sess) -> Vec<&'a Turn> {
    p.turns_of(s.session_id).collect()
}

fn attempts<'a>(p: &'a Projection, s: &Sess) -> Vec<&'a Attempt> {
    p.attempts_of(s.session_id).collect()
}

fn has_edge(p: &Projection, kind: EdgeKind, from: EdgeEndpoint, to: EdgeEndpoint) -> bool {
    p.edges
        .iter()
        .any(|e| e.kind == kind && e.from == from && e.to == to)
}

fn json(p: &Projection) -> String {
    serde_json::to_string(p).expect("projection serialises")
}

// --- reference scenario ----------------------------------------------------

#[test]
fn scenario_sessions_turns_and_coverage() {
    let sc = spec_scenario();
    let p = project(&sc.events);

    assert_eq!(p.algorithm_version, ALGORITHM_VERSION);
    assert_eq!(p.stats.events_seen, sc.events.len() as u64);
    assert_eq!(p.sessions.len(), 2, "two sessions");
    assert_eq!(
        p.sessions[0].session_id, sc.claude.session_id,
        "earlier session first"
    );
    assert_eq!(p.sessions[1].session_id, sc.codex.session_id);

    let claude = session(&p, &sc.claude);
    assert_eq!(claude.provider, Provider::ClaudeCode);
    assert_eq!(claude.project_id, sc.project_id);
    assert_eq!(claude.project_name, "acme/repo");
    assert_eq!(claude.started_at, at(0));
    assert_eq!(claude.ended_at, Some(at(80)));
    assert_eq!(claude.end_reason.as_deref(), Some("prompt_input_exit"));
    assert_eq!(claude.start_source.as_deref(), Some("startup"));
    assert_eq!(claude.coverage, CoverageGrade::Full);
    assert_eq!(claude.turn_count, 2);
    assert_eq!(claude.prompt_count, 2);
    assert_eq!(claude.tool_call_count, 5);
    assert_eq!(claude.failure_count, 1);
    assert_eq!(claude.agents.len(), 1);
    assert_eq!(claude.end_event_id, Some(sc.claude_end));
    assert_eq!(claude.last_event_id, sc.claude_end);
    assert_eq!(claude.last_event_at, at(80));

    let codex = session(&p, &sc.codex);
    assert_eq!(codex.provider, Provider::Codex);
    assert_eq!(codex.coverage, CoverageGrade::Full);
    assert_eq!(codex.turn_count, 1);
    assert_eq!(codex.tool_call_count, 2);
    assert_eq!(codex.failure_count, 0);

    assert_eq!(p.turns.len(), 3, "three turns overall");
    assert!(
        p.turns.iter().all(|t| t.prompt_event_id.is_some()),
        "no implicit turn"
    );
    let ct = turns(&p, &sc.claude);
    assert_eq!(ct.iter().map(|t| t.index).collect::<Vec<_>>(), vec![1, 2]);
    assert_eq!(ct[0].prompt_event_id, Some(sc.claude_prompt_1));
    assert_eq!(ct[0].stop_event_id, Some(sc.claude_stop_1));
    assert_eq!(ct[0].status, TurnStatus::Completed);
    assert_eq!(ct[0].started_at, at(5));
    assert_eq!(ct[0].ended_at, Some(at(45)));
    assert_eq!(
        ct[0].objective.as_deref(),
        Some("Fix the failing parser test")
    );
    assert_eq!(
        ct[0].prompt_chars,
        Some("Fix the failing parser test".len() as u64)
    );
    assert_eq!(ct[0].tool_call_ids.len(), 4);
    assert_eq!(ct[1].tool_call_ids.len(), 1);
    assert_eq!(
        ct[0].turn_id,
        attemptdb_core::TurnId::derive(&[&sc.claude.session_id.to_string(), "1"])
    );
}

#[test]
fn scenario_tool_pairing_and_durations() {
    let sc = spec_scenario();
    let p = project(&sc.events);
    let calls: Vec<_> = p.tool_calls_of(sc.claude.session_id).collect();
    assert_eq!(calls.len(), 5);
    assert!(
        calls
            .iter()
            .all(|c| c.started_at.is_some() && c.finished_at.is_some())
    );
    assert_eq!(p.stats.unpaired_tool_starts, 0);
    assert_eq!(p.stats.unpaired_tool_finishes, 0);
    assert_eq!(p.stats.fifo_pairings, 0);

    let read = calls[0];
    assert_eq!(read.tool.name, "Read");
    assert_eq!(read.duration_ms, Some(500));
    assert_eq!(
        read.outcome.as_ref().map(|o| o.status),
        Some(OutcomeStatus::Success)
    );

    let failed_edit = calls[1];
    assert_eq!(failed_edit.tool.call_id.as_deref(), Some("c2"));
    assert_eq!(failed_edit.start_event_id, Some(sc.edit_fail_start));
    assert_eq!(failed_edit.end_event_id, Some(sc.edit_fail_end));
    assert_eq!(failed_edit.duration_ms, Some(1_000));
    let outcome = failed_edit.outcome.as_ref().expect("outcome");
    assert_eq!(outcome.status, OutcomeStatus::Failure);
    assert_eq!(outcome.class.as_deref(), Some("string_mismatch"));
    assert_eq!(failed_edit.paths.len(), 1);
    assert_eq!(
        failed_edit.paths[0].repo_relative.as_deref(),
        Some("src/parser.rs")
    );

    let bash = calls[3];
    assert_eq!(bash.duration_ms, Some(15_000));
    assert_eq!(bash.outcome.as_ref().and_then(|o| o.exit_code), Some(0));
    assert_eq!(bash.turn_id, Some(turns(&p, &sc.claude)[0].turn_id));

    // Span ids derive from the call id, so they are stable across runs.
    let expected =
        attemptdb_core::SpanId::derive(&[&sc.claude.session_id.to_string(), "call", "c2"]);
    assert_eq!(failed_edit.tool_call_id, expected);
}

#[test]
fn scenario_attempts_and_supersession() {
    let sc = spec_scenario();
    let p = project(&sc.events);
    let ca = attempts(&p, &sc.claude);
    assert_eq!(ca.len(), 3, "two attempts in turn 1, one in turn 2");

    let (first, second, docs) = (ca[0], ca[1], ca[2]);
    assert_eq!((first.turn_index, first.index), (1, 0));
    assert_eq!((second.turn_index, second.index), (1, 1));
    assert_eq!((docs.turn_index, docs.index), (2, 0));

    assert_eq!(first.outcome, AttemptOutcome::Superseded);
    assert!(first.outcome.is_failure());
    assert_eq!(first.failure_class.as_deref(), Some("string_mismatch"));
    assert_eq!(first.superseded_by, Some(second.attempt_id));
    assert_eq!(first.tool_call_ids.len(), 2, "read + failed edit");
    assert_eq!(first.paths, vec!["src/parser.rs".to_string()]);
    assert_eq!(
        first.started_at,
        at(5),
        "first attempt starts with the prompt"
    );
    assert_eq!(first.ended_at, Some(at(11)), "ends with the failing edit");
    assert_eq!(first.approach, "edit src/parser.rs \u{b7} read");
    assert_eq!(
        first.objective.as_deref(),
        Some("Fix the failing parser test")
    );
    assert_eq!(first.confidence, 0.9);
    assert_eq!(first.algorithm_version, ALGORITHM_VERSION);
    assert!(first.evidence.contains(&sc.claude_prompt_1));
    assert!(first.evidence.contains(&sc.edit_fail_end));
    assert!(!first.evidence.contains(&sc.claude_stop_1));

    assert_eq!(second.outcome, AttemptOutcome::Succeeded);
    assert_eq!(second.supersedes, Some(first.attempt_id));
    assert_eq!(second.superseded_by, None);
    assert_eq!(second.started_at, at(20));
    assert_eq!(second.ended_at, Some(at(45)), "ends with the turn stop");
    assert_eq!(second.approach, "edit src/parser.rs \u{b7} shell");
    assert!(second.evidence.contains(&sc.claude_stop_1));
    assert!(second.evidence.contains(&sc.bash_end));
    assert_eq!(second.confidence, 0.9);

    assert_eq!(docs.outcome, AttemptOutcome::Succeeded);
    assert_eq!(docs.paths, vec!["README.md".to_string()]);
    assert_eq!(docs.supersedes, None);

    let xa = attempts(&p, &sc.codex);
    assert_eq!(xa.len(), 1);
    assert_eq!(xa[0].outcome, AttemptOutcome::Succeeded);
    assert_eq!(xa[0].approach, "edit src/parser.rs \u{b7} shell");

    // Attempt ids are derived, never random.
    assert_eq!(
        first.attempt_id,
        attemptdb_core::AttemptId::derive(&[&sc.claude.session_id.to_string(), "1", "0"])
    );
}

#[test]
fn scenario_handoff_and_edges() {
    let sc = spec_scenario();
    let p = project(&sc.events);

    assert_eq!(p.handoffs.len(), 1);
    let h = &p.handoffs[0];
    assert_eq!(h.from_session, sc.claude.session_id);
    assert_eq!(h.to_session, sc.codex.session_id);
    assert_eq!(h.from_provider, Provider::ClaudeCode);
    assert_eq!(h.to_provider, Provider::Codex);
    assert_eq!(h.project_id, sc.project_id);
    assert_eq!(h.at, at(260));
    assert_eq!(h.gap_ms, 180_000);
    assert_eq!(h.shared_paths, vec!["src/parser.rs".to_string()]);
    assert_eq!(h.confidence, 0.8);
    assert!(h.evidence.contains(&sc.claude_end));
    assert!(h.evidence.contains(&sc.codex_start));
    assert!(h.evidence.contains(&sc.codex_patch_start));

    let ca = attempts(&p, &sc.claude);
    let ct = turns(&p, &sc.claude);
    assert!(has_edge(
        &p,
        EdgeKind::Superseded,
        EdgeEndpoint::Attempt(ca[0].attempt_id),
        EdgeEndpoint::Attempt(ca[1].attempt_id)
    ));
    assert!(has_edge(
        &p,
        EdgeKind::Caused,
        EdgeEndpoint::Event(sc.edit_fail_end),
        EdgeEndpoint::Event(sc.edit_retry_start)
    ));
    assert!(has_edge(
        &p,
        EdgeKind::HandedOff,
        EdgeEndpoint::Session(sc.claude.session_id),
        EdgeEndpoint::Session(sc.codex.session_id)
    ));
    for t in &ct {
        assert!(has_edge(
            &p,
            EdgeKind::ParentOf,
            EdgeEndpoint::Session(sc.claude.session_id),
            EdgeEndpoint::Turn(t.turn_id)
        ));
        assert!(has_edge(
            &p,
            EdgeKind::Triggered,
            EdgeEndpoint::Event(t.prompt_event_id.unwrap()),
            EdgeEndpoint::Turn(t.turn_id)
        ));
        for span in &t.tool_call_ids {
            assert!(has_edge(
                &p,
                EdgeKind::ParentOf,
                EdgeEndpoint::Turn(t.turn_id),
                EdgeEndpoint::Span(*span)
            ));
        }
    }
    for a in &ca {
        for e in &a.evidence {
            assert!(has_edge(
                &p,
                EdgeKind::EvidenceFor,
                EdgeEndpoint::Event(*e),
                EdgeEndpoint::Attempt(a.attempt_id)
            ));
        }
    }
    assert!(
        p.edges.iter().all(|e| !e.evidence.is_empty()),
        "every edge cites evidence"
    );
}

#[test]
fn scenario_state_at_time_travel() {
    let sc = spec_scenario();
    let p = project(&sc.events);
    let ca = attempts(&p, &sc.claude);
    let ct = turns(&p, &sc.claude);

    // Before the session started: nothing is active.
    assert!(p.state_at(at(-1)).sessions.is_empty());

    // Right after the failed edit, before the retry.
    let snap = p.state_at(at(15));
    assert_eq!(snap.at, at(15));
    assert_eq!(snap.sessions.len(), 1);
    let st = &snap.sessions[0];
    assert_eq!(st.session_id, sc.claude.session_id);
    assert!(st.open);
    assert_eq!(st.current_turn, Some(ct[0].turn_id));
    assert_eq!(st.turn_index, Some(1));
    assert_eq!(st.turn_status, Some(TurnStatus::InProgress));
    assert!(st.in_flight_tool_calls.is_empty());
    assert_eq!(st.last_attempt, Some(ca[0].attempt_id));
    assert_eq!(
        st.last_attempt_outcome,
        Some(AttemptOutcome::Failed),
        "not yet superseded at t=15"
    );
    assert_eq!(st.last_failure_class.as_deref(), Some("string_mismatch"));
    assert_eq!(st.last_activity_at, at(11));
    assert!(!st.blocked);
    assert!(st.evidence.contains(&sc.edit_fail_end));

    // While the test run is in flight.
    let snap = p.state_at(at(30));
    let st = &snap.sessions[0];
    assert_eq!(st.in_flight_tool_calls.len(), 1);
    let bash = p
        .tool_calls_of(sc.claude.session_id)
        .find(|c| c.tool.name == "Bash")
        .unwrap();
    assert_eq!(st.in_flight_tool_calls[0], bash.tool_call_id);
    assert_eq!(st.last_attempt, Some(ca[1].attempt_id));
    assert_eq!(st.last_attempt_outcome, Some(AttemptOutcome::InProgress));
    assert!(st.evidence.contains(&sc.bash_start));

    // After the fix landed and the turn stopped.
    let snap = p.state_at(at(50));
    let st = &snap.sessions[0];
    assert_eq!(st.turn_status, Some(TurnStatus::Completed));
    assert_eq!(st.last_attempt, Some(ca[1].attempt_id));
    assert_eq!(st.last_attempt_outcome, Some(AttemptOutcome::Succeeded));
    assert!(st.in_flight_tool_calls.is_empty());

    // Claude has ended, Codex is running.
    let snap = p.state_at(at(275));
    assert_eq!(snap.sessions.len(), 1);
    assert_eq!(snap.sessions[0].session_id, sc.codex.session_id);
    assert_eq!(snap.sessions[0].turn_index, Some(1));

    // Exactly at the end timestamp the session is still reported, closed.
    let snap = p.state_at(at(80));
    assert_eq!(snap.sessions.len(), 1);
    assert!(!snap.sessions[0].open);
}

#[test]
fn scenario_healthy_sessions_are_not_blocked() {
    let sc = spec_scenario();
    let p = project(&sc.events);
    assert!(p.why_blocked(sc.claude.session_id).is_none());
    assert!(p.why_blocked(sc.codex.session_id).is_none());
    assert!(p.why_blocked(SessionId::derive(&["nope"])).is_none());
}

// --- determinism and ordering ----------------------------------------------

#[test]
fn determinism_identical_streams_give_identical_output() {
    let a = project(&spec_scenario().events);
    let b = project(&spec_scenario().events);
    assert_eq!(json(&a), json(&b));
    assert_eq!(a, b);
}

#[test]
fn out_of_order_events_are_sorted() {
    let sc = spec_scenario();
    let reference = project(&sc.events);

    let mut reversed = sc.events.clone();
    reversed.reverse();
    let p = project(&reversed);
    assert_eq!(p.stats.out_of_order_events, (sc.events.len() - 1) as u64);
    assert_eq!(
        json(&p),
        json(&reference).replacen(
            "\"out_of_order_events\":0",
            &format!("\"out_of_order_events\":{}", sc.events.len() - 1),
            1
        )
    );

    // A deterministic interleaving: odd positions first, then even.
    let mut shuffled: Vec<Event> = sc.events.iter().step_by(2).cloned().collect();
    shuffled.extend(sc.events.iter().skip(1).step_by(2).cloned());
    let mut p = project(&shuffled);
    p.stats.out_of_order_events = 0;
    assert_eq!(p, reference);
}

#[test]
fn projector_push_matches_project() {
    let sc = spec_scenario();
    let mut projector = Projector::new();
    assert!(projector.is_empty());
    for ev in &sc.events {
        projector.push(ev);
    }
    assert_eq!(projector.len(), sc.events.len());
    assert_eq!(projector.finish(), project(&sc.events));
}

#[test]
fn observed_time_leads_and_hlc_breaks_ties_when_every_event_is_ingested() {
    let sc = spec_scenario();
    let mut events = sc.events.clone();
    for (i, ev) in events.iter_mut().enumerate() {
        ev.hlc = Hlc::new(1_000 + i as u64, 0);
        ev.source_seq = i as u64 + 1;
    }
    let reference = project(&events);
    let i = events
        .iter()
        .position(|e| e.event_id == sc.edit_fail_start)
        .unwrap();
    let j = events
        .iter()
        .position(|e| e.event_id == sc.edit_fail_end)
        .unwrap();

    // Swapping only the HLCs changes nothing: observed time leads, so a
    // reconstructed event ingested much later still sorts where it happened.
    let mut hlc_swapped = events.clone();
    let (a, b) = (hlc_swapped[i].hlc, hlc_swapped[j].hlc);
    hlc_swapped[i].hlc = b;
    hlc_swapped[j].hlc = a;
    assert_eq!(project(&hlc_swapped), reference);

    // Swapping the observed times moves the end before the start: the end
    // becomes a lone finish and the start stays in flight.
    let mut wall_swapped = events.clone();
    let (a, b) = (wall_swapped[i].observed_at, wall_swapped[j].observed_at);
    wall_swapped[i].observed_at = b;
    wall_swapped[j].observed_at = a;
    let p = project(&wall_swapped);
    assert_ne!(p, reference);
    assert_eq!(p.stats.unpaired_tool_finishes, 1);
    assert_eq!(p.stats.unpaired_tool_starts, 1);

    // With equal observed times the HLC decides: start before end pairs,
    // end before start leaves both unpaired.
    let mut tied = events.clone();
    let t = tied[i].observed_at;
    tied[j].observed_at = t;
    let p = project(&tied);
    assert_eq!(p.stats.unpaired_tool_finishes, 0);
    assert_eq!(p.stats.unpaired_tool_starts, 0);
    let (a, b) = (tied[i].hlc, tied[j].hlc);
    tied[i].hlc = b;
    tied[j].hlc = a;
    let p = project(&tied);
    assert_eq!(p.stats.unpaired_tool_finishes, 1);
    assert_eq!(p.stats.unpaired_tool_starts, 1);
}

#[test]
fn projection_roundtrips_through_serde() {
    let p = project(&spec_scenario().events);
    let text = json(&p);
    let back: Projection = serde_json::from_str(&text).expect("deserialises");
    assert_eq!(back, p);
    assert!(text.contains("\"algorithm_version\":\"tier1-v0\""));

    let foreign = text.replace("\"tier1-v0\"", "\"tier1-v99\"");
    let err = serde_json::from_str::<Projection>(&foreign).unwrap_err();
    assert!(
        err.to_string()
            .contains("unsupported projection algorithm version")
    );
}

// --- tool call pairing ------------------------------------------------------

#[test]
fn fifo_pairing_without_call_ids() {
    let mut b = Stream::new();
    let s = Sess::claude("fifo");
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "run things");
    let first_start = b.tool_start(&s, at(2), &Tool::shell(None));
    let second_start = b.tool_start(&s, at(3), &Tool::shell(None));
    let read_start = b.tool_start(&s, at(4), &Tool::read(None, &["a.rs"]));
    let read_end = b.tool_finish(&s, at(5), &Tool::read(None, &["a.rs"]), Outcome::success());
    let first_end = b.tool_finish(&s, at(6), &Tool::shell(None), Outcome::success());
    let second_end = b.tool_failed(&s, at(9), &Tool::shell(None), "nonzero_exit");
    b.stop(&s, at(10));
    b.session_ended(&s, at(11), "exit");
    let p = project(&b.build());

    let calls: Vec<_> = p.tool_calls_of(s.session_id).collect();
    assert_eq!(calls.len(), 3);
    assert_eq!(p.stats.fifo_pairings, 3);
    assert_eq!(p.stats.unpaired_tool_starts, 0);
    assert_eq!(p.stats.unpaired_tool_finishes, 0);

    assert_eq!(calls[0].start_event_id, Some(first_start));
    assert_eq!(calls[0].end_event_id, Some(first_end));
    assert_eq!(calls[0].duration_ms, Some(4_000));
    assert_eq!(calls[1].start_event_id, Some(second_start));
    assert_eq!(calls[1].end_event_id, Some(second_end));
    assert_eq!(calls[1].duration_ms, Some(6_000));
    assert_eq!(
        calls[1].outcome.as_ref().and_then(|o| o.class.as_deref()),
        Some("nonzero_exit")
    );
    assert_eq!(calls[2].start_event_id, Some(read_start));
    assert_eq!(calls[2].end_event_id, Some(read_end));

    // Sequence-based span ids are derived from the session and ordinal.
    let sid = s.session_id.to_string();
    assert_eq!(
        calls[0].tool_call_id,
        attemptdb_core::SpanId::derive(&[&sid, "seq", "0"])
    );
    assert_eq!(
        calls[1].tool_call_id,
        attemptdb_core::SpanId::derive(&[&sid, "seq", "1"])
    );

    // FIFO pairing lowers attempt confidence.
    let a = attempts(&p, &s);
    assert_eq!(a.len(), 1, "the shell failure ends the only attempt");
    assert_eq!(a[0].outcome, AttemptOutcome::Failed);
    assert_eq!(a[0].failure_class.as_deref(), Some("nonzero_exit"));
    assert_eq!(a[0].confidence, 0.6);
}

#[test]
fn end_with_foreign_call_id_does_not_steal_an_identified_start() {
    let mut b = Stream::new();
    let s = Sess::claude("foreign-id");
    b.prompt(&s, at(1), "go");
    let identified = b.tool_start(&s, at(2), &Tool::shell(Some("known")));
    let stray_end = b.tool_finish(&s, at(3), &Tool::shell(Some("other")), Outcome::success());
    let known_end = b.tool_finish(&s, at(4), &Tool::shell(Some("known")), Outcome::success());
    b.stop(&s, at(5));
    let p = project(&b.build());

    let calls: Vec<_> = p.tool_calls_of(s.session_id).collect();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].start_event_id, Some(identified));
    assert_eq!(calls[0].end_event_id, Some(known_end));
    assert_eq!(calls[1].start_event_id, None);
    assert_eq!(calls[1].end_event_id, Some(stray_end));
    assert_eq!(p.stats.unpaired_tool_finishes, 1);
    assert_eq!(p.stats.fifo_pairings, 0);
}

#[test]
fn unpaired_start_is_in_flight_and_counted() {
    let mut b = Stream::new();
    let s = Sess::claude("inflight");
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "hang");
    let start = b.tool_start(&s, at(2), &Tool::shell(Some("c1")));
    let p = project(&b.build());

    assert_eq!(p.stats.unpaired_tool_starts, 1);
    let calls: Vec<_> = p.tool_calls_of(s.session_id).collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].start_event_id, Some(start));
    assert_eq!(calls[0].finished_at, None);
    assert_eq!(calls[0].outcome, None);
    assert_eq!(calls[0].duration_ms, None);

    let t = turns(&p, &s);
    assert_eq!(t[0].status, TurnStatus::InProgress);
    let a = attempts(&p, &s);
    assert_eq!(a[0].outcome, AttemptOutcome::InProgress);
    assert_eq!(a[0].ended_at, None);
    assert_eq!(a[0].confidence, 0.6, "missing stop and open pairing");

    let st = &p.state_at(at(100)).sessions[0];
    assert!(st.open);
    assert_eq!(st.in_flight_tool_calls, vec![calls[0].tool_call_id]);
    assert_eq!(st.last_attempt_outcome, Some(AttemptOutcome::InProgress));
    assert_eq!(session(&p, &s).coverage, CoverageGrade::Partial);
}

#[test]
fn lone_finish_becomes_a_complete_call() {
    let mut b = Stream::new();
    let s = Sess::codex("lone");
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "x");
    let end = b.tool_finish_bare(&s, at(2), &Tool::apply_patch(Some("p1"), &["src/a.rs"]));
    b.stop(&s, at(3));
    b.session_ended(&s, at(4), "exit");
    let p = project(&b.build());

    assert_eq!(p.stats.unpaired_tool_finishes, 1);
    let calls: Vec<_> = p.tool_calls_of(s.session_id).collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].started_at, None);
    assert_eq!(calls[0].finished_at, Some(at(2)));
    assert_eq!(calls[0].start_event_id, None);
    assert_eq!(calls[0].end_event_id, Some(end));
    assert_eq!(
        calls[0].outcome,
        Some(Outcome::success()),
        "bare finish defaults to success"
    );
    let a = attempts(&p, &s);
    assert_eq!(a[0].outcome, AttemptOutcome::Succeeded);
    assert_eq!(a[0].paths, vec!["src/a.rs".to_string()]);
    assert_eq!(a[0].confidence, 0.6);
}

// --- capture modes -----------------------------------------------------------

#[test]
fn metadata_only_stream_yields_attempts_without_objective() {
    let sc = spec_scenario_with(CaptureMode::MetadataOnly);
    assert!(sc.events.iter().all(|e| e.content.is_none()));
    let p = project(&sc.events);

    assert!(p.turns.iter().all(|t| t.objective.is_none()));
    assert!(p.attempts.iter().all(|a| a.objective.is_none()));
    let ct = turns(&p, &sc.claude);
    assert_eq!(
        ct[0].prompt_chars,
        Some("Fix the failing parser test".len() as u64)
    );

    let ca = attempts(&p, &sc.claude);
    assert_eq!(ca.len(), 3);
    assert_eq!(ca[0].outcome, AttemptOutcome::Superseded);
    assert_eq!(ca[1].outcome, AttemptOutcome::Succeeded);
    assert_eq!(ca[1].approach, "edit src/parser.rs \u{b7} shell");
    assert_eq!(p.handoffs.len(), 1);

    // Apart from the objective, the projection is identical.
    let mut full = project(&spec_scenario().events);
    for t in &mut full.turns {
        t.objective = None;
    }
    for a in &mut full.attempts {
        a.objective = None;
    }
    assert_eq!(p, full);
}

#[test]
fn approach_and_output_are_content_free() {
    let sc = spec_scenario();
    let p = project(&sc.events);
    let text = json(&p);
    assert!(
        !text.contains("cargo test"),
        "shell command text must not leak"
    );
    for a in &p.attempts {
        assert!(!a.approach.contains("Fix the failing"));
        assert!(!a.approach.contains("cargo"));
    }
    // Prompt text is allowed only in the objective field.
    let without_objectives = {
        let mut p = p.clone();
        for t in &mut p.turns {
            t.objective = None;
        }
        for a in &mut p.attempts {
            a.objective = None;
        }
        json(&p)
    };
    assert!(!without_objectives.contains("Fix the failing parser test"));
}

// --- turns -------------------------------------------------------------------

#[test]
fn implicit_turn_zero_holds_tool_events_before_any_prompt() {
    let mut b = Stream::new();
    let s = Sess::claude("implicit");
    b.session_started(&s, at(0));
    let start = b.tool_start(&s, at(1), &Tool::read(Some("r1"), &["x.rs"]));
    b.tool_finish(
        &s,
        at(2),
        &Tool::read(Some("r1"), &["x.rs"]),
        Outcome::success(),
    );
    let prompt = b.prompt(&s, at(3), "now do it");
    b.tool_start(&s, at(4), &Tool::edit(Some("e1"), &["x.rs"]));
    b.tool_finish(
        &s,
        at(5),
        &Tool::edit(Some("e1"), &["x.rs"]),
        Outcome::success(),
    );
    b.stop(&s, at(6));
    b.session_ended(&s, at(7), "exit");
    let p = project(&b.build());

    let t = turns(&p, &s);
    assert_eq!(t.len(), 2);
    assert_eq!(t[0].index, 0);
    assert_eq!(t[0].prompt_event_id, None);
    assert_eq!(t[0].objective, None);
    assert_eq!(t[0].started_at, at(1));
    assert_eq!(t[0].first_event_id, start);
    assert_eq!(t[0].ended_at, Some(at(3)), "cut by the first prompt");
    assert_eq!(t[0].status, TurnStatus::Unknown);
    assert_eq!(t[0].tool_call_ids.len(), 1);
    assert_eq!(t[1].index, 1);
    assert_eq!(t[1].prompt_event_id, Some(prompt));
    assert_eq!(t[1].status, TurnStatus::Completed);
    assert_eq!(session(&p, &s).turn_count, 2);
    assert_eq!(session(&p, &s).prompt_count, 1);

    let a = attempts(&p, &s);
    assert_eq!(a[0].outcome, AttemptOutcome::Abandoned);
    assert_eq!(a[1].outcome, AttemptOutcome::Succeeded);
    // ParentOf edges for the implicit turn cite its first event.
    let parent = p
        .edges
        .iter()
        .find(|e| e.kind == EdgeKind::ParentOf && e.to == EdgeEndpoint::Turn(t[0].turn_id))
        .unwrap();
    assert_eq!(parent.evidence, vec![start]);
}

#[test]
fn turn_cut_by_next_prompt_is_abandoned() {
    let mut b = Stream::new();
    let s = Sess::claude("cut");
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "first");
    b.tool_start(&s, at(2), &Tool::edit(Some("e1"), &["a.rs"]));
    b.tool_finish(
        &s,
        at(3),
        &Tool::edit(Some("e1"), &["a.rs"]),
        Outcome::success(),
    );
    b.prompt(&s, at(10), "second, interrupting");
    b.tool_start(&s, at(11), &Tool::edit(Some("e2"), &["a.rs"]));
    b.tool_finish(
        &s,
        at(12),
        &Tool::edit(Some("e2"), &["a.rs"]),
        Outcome::success(),
    );
    b.stop(&s, at(13));
    b.session_ended(&s, at(14), "exit");
    let p = project(&b.build());

    let t = turns(&p, &s);
    assert_eq!(t[0].status, TurnStatus::Unknown);
    assert_eq!(t[0].ended_at, Some(at(10)));
    assert_eq!(t[0].stop_event_id, None);
    let a = attempts(&p, &s);
    assert_eq!(a[0].outcome, AttemptOutcome::Abandoned);
    assert_eq!(a[0].ended_at, Some(at(10)));
    assert_eq!(a[0].confidence, 0.6, "no explicit stop");
    // An abandoned (not failed) attempt is not superseded even on the same path.
    assert_eq!(a[0].superseded_by, None);
    assert_eq!(a[1].outcome, AttemptOutcome::Succeeded);
}

#[test]
fn session_end_without_stop_abandons_the_open_turn() {
    let mut b = Stream::new();
    let s = Sess::claude("ended-mid-turn");
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "first");
    b.tool_start(&s, at(2), &Tool::shell(Some("c1")));
    b.tool_finish(&s, at(3), &Tool::shell(Some("c1")), Outcome::success());
    b.session_ended(&s, at(4), "other");
    let p = project(&b.build());
    let t = turns(&p, &s);
    assert_eq!(t[0].status, TurnStatus::Unknown);
    assert_eq!(t[0].ended_at, Some(at(4)));
    assert_eq!(attempts(&p, &s)[0].outcome, AttemptOutcome::Abandoned);
}

#[test]
fn turn_failed_marks_last_attempt_failed_and_prompt_only_turn_is_unknown() {
    let mut b = Stream::new();
    let s = Sess::claude("turn-failed");
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "first");
    b.tool_start(&s, at(2), &Tool::shell(Some("c1")));
    b.tool_finish(&s, at(3), &Tool::shell(Some("c1")), Outcome::success());
    b.turn_failed(&s, at(4), "rate_limited");
    b.prompt(&s, at(5), "just chatting");
    b.stop(&s, at(6));
    b.session_ended(&s, at(7), "exit");
    let p = project(&b.build());

    let t = turns(&p, &s);
    assert_eq!(t[0].status, TurnStatus::Failed);
    assert_eq!(t[1].status, TurnStatus::Completed);
    assert_eq!(session(&p, &s).failure_count, 1);
    let a = attempts(&p, &s);
    assert_eq!(a.len(), 2);
    assert_eq!(a[0].outcome, AttemptOutcome::Failed);
    assert_eq!(a[0].failure_class.as_deref(), Some("rate_limited"));
    assert_eq!(
        a[1].outcome,
        AttemptOutcome::Unknown,
        "stopped turn without tool calls"
    );
    assert_eq!(a[1].approach, "no tool calls");
    assert!(a[1].tool_call_ids.is_empty());
}

#[test]
fn multiple_stops_move_the_turn_end_forward() {
    let mut b = Stream::new();
    let s = Sess::claude("double-stop");
    b.prompt(&s, at(1), "go");
    let first_stop = b.stop(&s, at(2));
    b.tool_start(&s, at(3), &Tool::shell(Some("c1")));
    b.tool_finish(&s, at(4), &Tool::shell(Some("c1")), Outcome::success());
    let second_stop = b.stop(&s, at(5));
    let p = project(&b.build());
    let t = turns(&p, &s);
    assert_eq!(t.len(), 1);
    assert_ne!(first_stop, second_stop);
    assert_eq!(t[0].stop_event_id, Some(second_stop));
    assert_eq!(t[0].ended_at, Some(at(5)));
    assert_eq!(t[0].tool_call_ids.len(), 1);
    assert_eq!(attempts(&p, &s)[0].outcome, AttemptOutcome::Succeeded);
}

// --- blocked heuristics ------------------------------------------------------

#[test]
fn why_blocked_on_trailing_permission_request() {
    let mut b = Stream::new();
    let s = Sess::claude("perm");
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "delete the build dir");
    b.tool_start(&s, at(2), &Tool::read(Some("r1"), &["Makefile"]));
    b.tool_finish(
        &s,
        at(3),
        &Tool::read(Some("r1"), &["Makefile"]),
        Outcome::success(),
    );
    let perm = b.permission_requested(&s, at(4), &Tool::shell(Some("c2")));
    let p = project(&b.build());

    let why = p.why_blocked(s.session_id).expect("blocked");
    assert!(why.claim.contains("permission request"), "{}", why.claim);
    assert_eq!(why.evidence, vec![perm]);
    assert_eq!(why.confidence, 0.65, "coverage is partial (no session end)");
    assert!(why.uncertainty.contains("partial"));
    assert!(why.uncertainty.contains("no session end"));

    let st = &p.state_at(at(4)).sessions[0];
    assert!(st.blocked);
    assert_eq!(
        st.block.as_ref().map(|e| e.claim.clone()),
        Some(why.claim.clone())
    );
    assert!(st.evidence.contains(&perm));
    // Before the request was raised the session was not blocked.
    assert!(!p.state_at(at(3)).sessions[0].blocked);

    assert_eq!(p.signals.len(), 1);
    assert_eq!(p.signals[0].event_id, perm);
    assert_eq!(p.signals[0].cleared_at, None);
}

#[test]
fn permission_request_followed_by_activity_is_not_blocked() {
    let mut b = Stream::new();
    let s = Sess::claude("perm-cleared");
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "do it");
    let perm = b.permission_requested(&s, at(2), &Tool::shell(Some("c1")));
    let start = b.tool_start(&s, at(3), &Tool::shell(Some("c1")));
    b.tool_finish(&s, at(4), &Tool::shell(Some("c1")), Outcome::success());
    b.stop(&s, at(5));
    b.session_ended(&s, at(6), "exit");
    let p = project(&b.build());

    assert!(p.why_blocked(s.session_id).is_none());
    assert!(
        p.state_at(at(2)).sessions[0].blocked,
        "blocked while the request was pending"
    );
    assert!(!p.state_at(at(3)).sessions[0].blocked);
    assert_eq!(p.signals[0].event_id, perm);
    assert_eq!(p.signals[0].cleared_at, Some(at(3)));
    assert_eq!(p.signals[0].cleared_by, Some(start));

    let why_full = {
        let mut b = Stream::new();
        let s2 = Sess::claude("perm-full");
        b.session_started(&s2, at(0));
        b.prompt(&s2, at(1), "x");
        b.tool_start(&s2, at(2), &Tool::shell(Some("c1")));
        b.tool_finish(&s2, at(3), &Tool::shell(Some("c1")), Outcome::success());
        b.session_ended(&s2, at(4), "exit");
        b.permission_requested(&s2, at(5), &Tool::shell(Some("c2")));
        let p = project(&b.build());
        p.why_blocked(s2.session_id).expect("blocked")
    };
    assert_eq!(why_full.confidence, 0.85, "full coverage");
    assert!(why_full.uncertainty.contains("Coverage is full"));
}

#[test]
fn blocking_notification_types_are_signals_and_others_are_not() {
    for ty in ["permission_prompt", "idle_prompt", "agent_needs_input"] {
        let mut b = Stream::new();
        let s = Sess::claude(&format!("notif-{ty}"));
        b.prompt(&s, at(1), "x");
        let n = b.notification(&s, at(2), ty);
        let p = project(&b.build());
        let why = p
            .why_blocked(s.session_id)
            .unwrap_or_else(|| panic!("{ty} should block"));
        assert!(why.claim.contains(ty), "{}", why.claim);
        assert_eq!(why.evidence, vec![n]);
        assert_eq!(p.signals[0].signal_type.as_deref(), Some(ty));
    }
    let mut b = Stream::new();
    let s = Sess::claude("notif-info");
    b.prompt(&s, at(1), "x");
    b.notification(&s, at(2), "info");
    let p = project(&b.build());
    assert!(p.why_blocked(s.session_id).is_none());
    assert!(p.signals.is_empty());
}

#[test]
fn why_blocked_on_repeated_failures_with_same_class() {
    let mut b = Stream::new();
    let s = Sess::claude("repeat");
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "fix it");
    b.tool_start(&s, at(2), &Tool::edit(Some("e1"), &["src/x.rs"]));
    let f1 = b.tool_failed(
        &s,
        at(3),
        &Tool::edit(Some("e1"), &["src/x.rs"]),
        "string_mismatch",
    );
    b.tool_start(&s, at(4), &Tool::edit(Some("e2"), &["src/x.rs"]));
    let f2 = b.tool_failed(
        &s,
        at(5),
        &Tool::edit(Some("e2"), &["src/x.rs"]),
        "string_mismatch",
    );
    b.stop(&s, at(6));
    b.session_ended(&s, at(7), "exit");
    let p = project(&b.build());

    let a = attempts(&p, &s);
    assert_eq!(a.len(), 2);
    assert_eq!(a[0].outcome, AttemptOutcome::Superseded);
    assert_eq!(a[0].superseded_by, Some(a[1].attempt_id));
    assert_eq!(a[1].outcome, AttemptOutcome::Failed);
    assert_eq!(a[1].supersedes, Some(a[0].attempt_id));

    let why = p.why_blocked(s.session_id).expect("blocked");
    assert!(why.claim.contains("string_mismatch"), "{}", why.claim);
    assert!(why.evidence.contains(&f1) && why.evidence.contains(&f2));
    assert_eq!(why.confidence, 0.7);
    assert!(why.uncertainty.contains("Coverage is full"));

    // Different classes do not count as repetition.
    let mut b = Stream::new();
    let s2 = Sess::claude("repeat-different");
    b.prompt(&s2, at(1), "fix it");
    b.tool_start(&s2, at(2), &Tool::edit(Some("e1"), &["src/x.rs"]));
    b.tool_failed(
        &s2,
        at(3),
        &Tool::edit(Some("e1"), &["src/x.rs"]),
        "string_mismatch",
    );
    b.tool_start(&s2, at(4), &Tool::shell(Some("c1")));
    b.tool_failed(&s2, at(5), &Tool::shell(Some("c1")), "nonzero_exit");
    b.stop(&s2, at(6));
    let p = project(&b.build());
    assert!(p.why_blocked(s2.session_id).is_none());
}

#[test]
fn calls_running_when_a_failure_lands_stay_in_the_failed_attempt() {
    let mut b = Stream::new();
    let s = Sess::claude("concurrent");
    b.prompt(&s, at(1), "go");
    b.tool_start(&s, at(2), &Tool::shell(Some("bg")));
    b.tool_start(&s, at(3), &Tool::edit(Some("e1"), &["src/x.rs"]));
    b.tool_failed(
        &s,
        at(4),
        &Tool::edit(Some("e1"), &["src/x.rs"]),
        "string_mismatch",
    );
    b.tool_finish(&s, at(5), &Tool::shell(Some("bg")), Outcome::success());
    b.tool_start(&s, at(6), &Tool::edit(Some("e2"), &["src/x.rs"]));
    b.tool_finish(
        &s,
        at(7),
        &Tool::edit(Some("e2"), &["src/x.rs"]),
        Outcome::success(),
    );
    b.stop(&s, at(8));
    let p = project(&b.build());
    let a = attempts(&p, &s);
    assert_eq!(a.len(), 2);
    assert_eq!(
        a[0].tool_call_ids.len(),
        2,
        "the background shell belongs to the failed attempt"
    );
    assert_eq!(a[0].outcome, AttemptOutcome::Superseded);
    assert_eq!(a[0].ended_at, Some(at(4)));
    assert_eq!(a[0].approach, "edit src/x.rs \u{b7} shell");
    assert_eq!(a[1].tool_call_ids.len(), 1);
    assert_eq!(a[1].outcome, AttemptOutcome::Succeeded);
    assert_eq!(a[1].started_at, at(6));
}

#[test]
fn denied_shell_ends_attempt_with_denied_class() {
    let mut b = Stream::new();
    let s = Sess::claude("denied");
    b.prompt(&s, at(1), "rm -rf");
    b.tool_start(&s, at(2), &Tool::shell(Some("c1")));
    b.tool_denied(&s, at(3), &Tool::shell(Some("c1")));
    b.tool_start(&s, at(4), &Tool::read(Some("r1"), &["a.rs"]));
    b.tool_failed(
        &s,
        at(5),
        &Tool::read(Some("r1"), &["a.rs"]),
        "file_not_found",
    );
    b.stop(&s, at(6));
    let p = project(&b.build());
    let a = attempts(&p, &s);
    assert_eq!(a.len(), 2);
    assert_eq!(a[0].outcome, AttemptOutcome::Failed);
    assert_eq!(a[0].failure_class.as_deref(), Some("denied"));
    // A failed read does not split attempts.
    assert_eq!(a[1].outcome, AttemptOutcome::Succeeded);
    assert_eq!(session(&p, &s).failure_count, 2);
}

// --- coverage ----------------------------------------------------------------

#[test]
fn coverage_grades() {
    let mut b = Stream::new();
    let minimal = Sess::claude("minimal");
    b.tool_start(&minimal, at(1), &Tool::shell(Some("c1")));
    b.tool_finish(
        &minimal,
        at(2),
        &Tool::shell(Some("c1")),
        Outcome::success(),
    );

    let prompts_only = Sess::claude("prompts-only");
    b.prompt(&prompts_only, at(10), "hello");
    b.stop(&prompts_only, at(11));

    let partial = Sess::claude("partial");
    b.session_started(&partial, at(20));
    b.prompt(&partial, at(21), "x");
    b.tool_start(&partial, at(22), &Tool::shell(Some("c1")));
    b.tool_finish(
        &partial,
        at(23),
        &Tool::shell(Some("c1")),
        Outcome::success(),
    );
    b.stop(&partial, at(24));

    let lifecycle_no_tools = Sess::claude("no-tools");
    b.session_started(&lifecycle_no_tools, at(30));
    b.prompt(&lifecycle_no_tools, at(31), "x");
    b.stop(&lifecycle_no_tools, at(32));
    b.session_ended(&lifecycle_no_tools, at(33), "exit");

    let unknown = Sess::claude("unknown");
    b.session_started(&unknown, at(40));
    b.notification(&unknown, at(41), "info");
    b.session_ended(&unknown, at(42), "exit");

    let p = project(&b.build());
    assert_eq!(session(&p, &minimal).coverage, CoverageGrade::Minimal);
    assert_eq!(
        session(&p, &minimal).started_at,
        at(1),
        "first event when no start"
    );
    assert_eq!(session(&p, &prompts_only).coverage, CoverageGrade::Minimal);
    assert_eq!(session(&p, &partial).coverage, CoverageGrade::Partial);
    assert_eq!(
        session(&p, &lifecycle_no_tools).coverage,
        CoverageGrade::Partial
    );
    assert_eq!(session(&p, &unknown).coverage, CoverageGrade::Unknown);
    assert_eq!(session(&p, &unknown).turn_count, 0);
    assert!(attempts(&p, &unknown).is_empty());

    assert_eq!(attempts(&p, &minimal)[0].confidence, 0.4);
    let st = p.state_at(at(41));
    let u = st
        .sessions
        .iter()
        .find(|s| s.session_id == unknown.session_id)
        .unwrap();
    assert_eq!(u.current_turn, None);
    assert_eq!(u.last_attempt, None);
    assert!(!u.blocked);
}

// --- handoffs ----------------------------------------------------------------

fn two_sessions(from: &Sess, to: &Sess, gap_secs: i64, to_paths: &[&str]) -> Vec<Event> {
    let mut b = Stream::new();
    b.session_started(from, at(0));
    b.prompt(from, at(1), "a");
    b.tool_start(from, at(2), &Tool::edit(Some("a1"), &["src/a.rs"]));
    b.tool_finish(
        from,
        at(3),
        &Tool::edit(Some("a1"), &["src/a.rs"]),
        Outcome::success(),
    );
    b.stop(from, at(4));
    b.session_ended(from, at(5), "exit");
    let t0 = 5 + gap_secs;
    b.session_started(to, at(t0));
    b.prompt(to, at(t0 + 1), "b");
    b.tool_start(to, at(t0 + 2), &Tool::apply_patch(Some("b1"), to_paths));
    b.tool_finish(
        to,
        at(t0 + 3),
        &Tool::apply_patch(Some("b1"), to_paths),
        Outcome::success(),
    );
    b.stop(to, at(t0 + 4));
    b.session_ended(to, at(t0 + 5), "exit");
    b.build()
}

#[test]
fn handoff_variants() {
    let claude = Sess::claude("h-claude");
    let codex = Sess::codex("h-codex");

    // Within 5 minutes, no shared path: low confidence.
    let p = project(&two_sessions(&claude, &codex, 4 * 60, &["src/other.rs"]));
    assert_eq!(p.handoffs.len(), 1);
    assert_eq!(p.handoffs[0].confidence, 0.5);
    assert!(p.handoffs[0].shared_paths.is_empty());
    assert_eq!(p.handoffs[0].gap_ms, 240_000);

    // Within 30 minutes with a shared path: high confidence.
    let p = project(&two_sessions(&claude, &codex, 20 * 60, &["src/a.rs"]));
    assert_eq!(p.handoffs.len(), 1);
    assert_eq!(p.handoffs[0].confidence, 0.8);
    assert_eq!(p.handoffs[0].shared_paths, vec!["src/a.rs".to_string()]);

    // Within 30 minutes without a shared path: nothing.
    let p = project(&two_sessions(&claude, &codex, 20 * 60, &["src/other.rs"]));
    assert!(p.handoffs.is_empty());

    // Beyond 30 minutes even with a shared path: nothing.
    let p = project(&two_sessions(&claude, &codex, 31 * 60, &["src/a.rs"]));
    assert!(p.handoffs.is_empty());

    // Same provider is a continuation, not a handoff.
    let claude2 = Sess::claude("h-claude-2");
    let p = project(&two_sessions(&claude, &claude2, 60, &["src/a.rs"]));
    assert!(p.handoffs.is_empty());
    assert!(!p.edges.iter().any(|e| e.kind == EdgeKind::HandedOff));

    // The receiving session may start while the giver is still open, as long
    // as the giver had no further activity.
    let mut b = Stream::new();
    b.session_started(&claude, at(0));
    b.prompt(&claude, at(1), "a");
    b.tool_start(&claude, at(2), &Tool::edit(Some("a1"), &["src/a.rs"]));
    b.tool_finish(
        &claude,
        at(3),
        &Tool::edit(Some("a1"), &["src/a.rs"]),
        Outcome::success(),
    );
    b.stop(&claude, at(4));
    b.session_started(&codex, at(60));
    b.prompt(&codex, at(61), "b");
    b.tool_start(
        &codex,
        at(62),
        &Tool::apply_patch(Some("b1"), &["src/a.rs"]),
    );
    b.tool_finish(
        &codex,
        at(63),
        &Tool::apply_patch(Some("b1"), &["src/a.rs"]),
        Outcome::success(),
    );
    b.stop(&codex, at(64));
    b.session_ended(&claude, at(120), "exit");
    let p = project(&b.build());
    assert_eq!(p.handoffs.len(), 1);
    assert_eq!(
        p.handoffs[0].gap_ms, 56_000,
        "measured from the giver's last activity"
    );
    assert_eq!(p.handoffs[0].confidence, 0.8);
}

// --- misc --------------------------------------------------------------------

#[test]
fn unknown_events_are_counted_not_dropped() {
    let mut b = Stream::new();
    let s = Sess::claude("unknown-kind");
    b.session_started(&s, at(0));
    b.unknown(&s, at(1));
    b.prompt(&s, at(2), "x");
    b.unknown(&s, at(3));
    b.stop(&s, at(4));
    let p = project(&b.build());
    assert_eq!(p.stats.unknown_events, 2);
    assert_eq!(session(&p, &s).event_count, 5);
    let t = turns(&p, &s);
    assert_eq!(t.len(), 1, "unknown events never create turns");
    assert_eq!(t[0].last_event_id, p.sessions[0].last_event_id);
}

#[test]
fn empty_stream_projects_to_nothing() {
    let p = project(std::iter::empty::<&Event>());
    assert!(p.sessions.is_empty());
    assert!(p.edges.is_empty());
    assert_eq!(p.stats.events_seen, 0);
    assert!(p.state_at(Timestamp::now()).sessions.is_empty());
    assert_eq!(p.algorithm_version, "tier1-v0");
    assert_eq!(p.algorithm_version.as_str(), ALGORITHM_VERSION);
    assert_eq!(&*p.algorithm_version, ALGORITHM_VERSION);
}
