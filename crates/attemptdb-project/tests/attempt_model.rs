//! The attempt model: what ends an attempt, what makes a retry, how parallel
//! agents are kept apart, and how an attempt keeps its identity (so that a
//! correction or a retraction lands on the attempt a person meant).

mod common;

use attemptdb_core::{AgentId, AttemptId, Event, EventId, Outcome};
use attemptdb_project::{
    AttemptOutcome, CorrectionStatus, DEFAULT_MIN_CONFIDENCE, Projection, Projector, Verification,
    project,
};
use common::{Sess, Stream, Tool, at};
use serde_json::json;

fn failing_test(
    b: &mut Stream,
    s: &Sess,
    t: i64,
    call: &str,
) -> (attemptdb_core::EventId, attemptdb_core::EventId) {
    b.shell_classified(
        s,
        at(t),
        at(t + 5),
        call,
        "test",
        None,
        Outcome::failure(Some("nonzero_exit".into())),
    )
}

fn passing_test(b: &mut Stream, s: &Sess, t: i64, call: &str) {
    b.shell_classified(s, at(t), at(t + 5), call, "test", None, Outcome::success());
}

fn edit_ok(b: &mut Stream, s: &Sess, t: i64, call: &str, path: &str) {
    let paths = [path];
    b.tool_start(s, at(t), &Tool::edit(Some(call), &paths));
    b.tool_finish(
        s,
        at(t + 1),
        &Tool::edit(Some(call), &paths),
        Outcome::success(),
    );
}

#[test]
fn a_failing_exploration_command_does_not_end_an_attempt() {
    let s = Sess::claude("explore");
    let mut b = Stream::new();
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "find the bug");
    // `grep` finds nothing and exits 1; `ls` of a missing path exits 2.
    for (i, class) in ["nonzero_exit", "nonzero_exit", "nonzero_exit"]
        .iter()
        .enumerate()
    {
        let id = format!("g{i}");
        b.tool_start(&s, at(5 + 10 * i as i64), &Tool::shell(Some(&id)));
        b.tool_failed(&s, at(6 + 10 * i as i64), &Tool::shell(Some(&id)), class);
    }
    edit_ok(&mut b, &s, 50, "e1", "src/a.rs");
    b.stop(&s, at(60));
    let p = project(&b.build());

    assert_eq!(p.attempts.len(), 1, "{:#?}", p.attempts);
    assert_eq!(p.attempts[0].outcome, AttemptOutcome::Succeeded);
    assert_eq!(p.sessions[0].failure_count, 3, "the calls still failed");
    assert!(
        p.attention_at(at(70), DEFAULT_MIN_CONFIDENCE).is_empty(),
        "three failed greps are not a loop to break"
    );
    assert!(p.decisions.is_empty());
}

#[test]
fn a_failing_shell_command_after_an_edit_or_a_failing_test_does_end_one() {
    // After an edit in the same attempt: the command is checking the edit.
    let s = Sess::claude("after-edit");
    let mut b = Stream::new();
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "fix");
    edit_ok(&mut b, &s, 5, "e1", "src/a.rs");
    b.tool_start(&s, at(10), &Tool::shell(Some("s1")));
    b.tool_failed(&s, at(11), &Tool::shell(Some("s1")), "nonzero_exit");
    edit_ok(&mut b, &s, 20, "e2", "src/a.rs");
    b.stop(&s, at(30));
    let p = project(&b.build());
    assert_eq!(p.attempts.len(), 2);
    assert_eq!(p.attempts[0].outcome, AttemptOutcome::Superseded);

    // A failing test run ends an attempt even before any edit.
    let s = Sess::claude("red");
    let mut b = Stream::new();
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "make the test pass");
    failing_test(&mut b, &s, 5, "t1");
    edit_ok(&mut b, &s, 20, "e1", "src/a.rs");
    passing_test(&mut b, &s, 25, "t2");
    b.stop(&s, at(40));
    let p = project(&b.build());
    assert_eq!(p.attempts.len(), 2);
    let (red, green) = (&p.attempts[0], &p.attempts[1]);
    assert_eq!(red.verification, Some(Verification::Failed));
    assert_eq!(
        red.outcome,
        AttemptOutcome::Superseded,
        "a later passing run of the same check is the retry that worked"
    );
    assert_eq!(red.superseded_by, Some(green.attempt_id));
    assert_eq!(green.outcome, AttemptOutcome::Succeeded);
    assert_eq!(green.verification, Some(Verification::Passed));
}

#[test]
fn red_green_cycles_that_end_green_are_not_a_loop_but_ones_that_do_not_are() {
    let s = Sess::claude("cycles");
    let build = |green_at_the_end: bool| {
        let mut b = Stream::new();
        b.session_started(&s, at(0));
        b.prompt(&s, at(1), "make the tests pass");
        for i in 0..3i64 {
            let t = 10 + i * 100;
            edit_ok(&mut b, &s, t, &format!("e{i}"), "src/a.rs");
            if i == 2 && green_at_the_end {
                passing_test(&mut b, &s, t + 5, &format!("t{i}"));
            } else {
                failing_test(&mut b, &s, t + 5, &format!("t{i}"));
            }
        }
        b.stop(&s, at(400));
        project(&b.build())
    };
    let p = build(true);
    let outcomes: Vec<_> = p.attempts.iter().map(|a| a.outcome).collect();
    assert_eq!(
        outcomes,
        vec![
            AttemptOutcome::Superseded,
            AttemptOutcome::Superseded,
            AttemptOutcome::Succeeded
        ]
    );
    assert!(p.attention_at(at(450), DEFAULT_MIN_CONFIDENCE).is_empty());
    assert!(p.why_blocked(s.session_id).is_none());

    let p = build(false);
    let queue = p.attention_at(at(450), DEFAULT_MIN_CONFIDENCE);
    assert_eq!(queue.len(), 1, "still red after three tries is a loop");
    assert_eq!(queue[0].failure_class.as_deref(), Some("nonzero_exit"));
}

#[test]
fn a_test_run_that_exits_zero_but_reports_failures_is_a_failed_verification() {
    let s = Sess::claude("quiet-failure");
    let mut b = Stream::new();
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "go");
    edit_ok(&mut b, &s, 5, "e1", "src/a.rs");
    let (_, end) = b.shell_classified(&s, at(10), at(15), "t1", "test", None, Outcome::success());
    for e in b.events.iter_mut().filter(|e| e.event_id == end) {
        e.attrs.insert("tests_failed".into(), json!(2));
    }
    b.stop(&s, at(30));
    let p = project(&b.build());
    let a = &p.attempts[0];
    assert_eq!(a.outcome, AttemptOutcome::Failed);
    assert_eq!(a.failure_class.as_deref(), Some("tests_failed"));
    assert_eq!(a.verification, Some(Verification::Failed));
    let call = p
        .tool_calls
        .iter()
        .find(|c| c.tests_failed.is_some())
        .expect("the test call");
    assert_eq!(call.tests_failed, Some(2));
}

#[test]
fn only_edited_paths_make_a_retry() {
    // Read-only overlap: a shell command times out, then Cargo.toml is
    // edited. That is not "gave up after the timeout and retried".
    let s = Sess::claude("read-overlap");
    let mut b = Stream::new();
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "run the tests");
    b.tool_start(&s, at(5), &Tool::read(Some("r1"), &["Cargo.toml"]));
    b.tool_finish(
        &s,
        at(6),
        &Tool::read(Some("r1"), &["Cargo.toml"]),
        Outcome::success(),
    );
    failing_test(&mut b, &s, 10, "t1");
    edit_ok(&mut b, &s, 30, "e1", "Cargo.toml");
    b.stop(&s, at(60));
    let p = project(&b.build());
    assert!(p.decisions.is_empty(), "{:?}", p.decisions);
    assert_eq!(
        p.attempts[0].outcome,
        AttemptOutcome::Failed,
        "the failed test run was not retried by a later passing check"
    );

    // A failed edit followed by an attempt that only *reads* that file.
    let mut b = Stream::new();
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "fix a.rs");
    b.tool_start(&s, at(5), &Tool::edit(Some("e1"), &["src/a.rs"]));
    b.tool_failed(
        &s,
        at(6),
        &Tool::edit(Some("e1"), &["src/a.rs"]),
        "string_mismatch",
    );
    b.tool_start(&s, at(10), &Tool::read(Some("r"), &["src/a.rs"]));
    b.tool_finish(
        &s,
        at(11),
        &Tool::read(Some("r"), &["src/a.rs"]),
        Outcome::success(),
    );
    edit_ok(&mut b, &s, 20, "e2", "src/b.rs");
    b.stop(&s, at(30));
    let p = project(&b.build());
    assert_eq!(p.attempts[0].outcome, AttemptOutcome::Failed);
    assert_eq!(p.attempts[0].superseded_by, None);
    assert!(p.decisions.is_empty());

    // The same edit retried is a retry.
    let mut b = Stream::new();
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "fix a.rs");
    b.tool_start(&s, at(5), &Tool::edit(Some("e1"), &["src/a.rs"]));
    b.tool_failed(
        &s,
        at(6),
        &Tool::edit(Some("e1"), &["src/a.rs"]),
        "string_mismatch",
    );
    edit_ok(&mut b, &s, 20, "e2", "src/a.rs");
    b.stop(&s, at(30));
    let p = project(&b.build());
    assert_eq!(p.attempts[0].outcome, AttemptOutcome::Superseded);
    assert_eq!(p.decisions.len(), 1);
}

#[test]
fn parallel_agents_do_not_share_an_attempt_stream() {
    let s = Sess::claude("parallel");
    let mut b = Stream::new();
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "do two things in parallel");
    let steps = [
        ("A1", "src/a.rs"),
        ("B1", "src/b.rs"),
        ("A2", "src/a.rs"),
        ("B2", "src/b.rs"),
    ];
    let mut agent_of: std::collections::HashMap<EventId, char> = Default::default();
    for (i, (call, path)) in steps.iter().enumerate() {
        let t = 10 + i as i64 * 10;
        let paths = [*path];
        let start = b.tool_start(&s, at(t), &Tool::edit(Some(call), &paths));
        let end = if *call == "A1" {
            b.tool_failed(
                &s,
                at(t + 5),
                &Tool::edit(Some(call), &paths),
                "string_mismatch",
            )
        } else {
            b.tool_finish(
                &s,
                at(t + 5),
                &Tool::edit(Some(call), &paths),
                Outcome::success(),
            )
        };
        let agent = call.chars().next().unwrap();
        agent_of.insert(start, agent);
        agent_of.insert(end, agent);
    }
    b.stop(&s, at(100));
    let mut events = b.build();
    let (aa, bb) = (
        AgentId::derive(&["sub", "A"]),
        AgentId::derive(&["sub", "B"]),
    );
    for e in events.iter_mut() {
        if let Some(c) = agent_of.get(&e.event_id) {
            e.agent.agent_id = if *c == 'A' { aa } else { bb };
        }
    }
    let p = project(&events);

    assert_eq!(p.attempts.len(), 3, "{:#?}", p.attempts);
    let of =
        |agent: AgentId| -> Vec<_> { p.attempts.iter().filter(|a| a.agent_id == agent).collect() };
    let (a, bq) = (of(aa), of(bb));
    assert_eq!(a.len(), 2, "A failed once and retried");
    assert_eq!(bq.len(), 1, "A's failure did not end B's attempt");
    assert_eq!(bq[0].tool_call_ids.len(), 2);
    assert_eq!(bq[0].outcome, AttemptOutcome::Succeeded);
    assert_eq!(a[0].outcome, AttemptOutcome::Superseded);
    assert_eq!(a[0].superseded_by, Some(a[1].attempt_id));
    assert_eq!(p.work_units[0].failure_count, 1);
}

// ---------------------------------------------------------------------------
// Identity
// ---------------------------------------------------------------------------

fn turn_with_two_edits(late_first_failure: bool, s: &Sess) -> Stream {
    let mut b = Stream::new();
    b.session_started(s, at(0));
    b.prompt(s, at(1), "fix");
    b.tool_start(s, at(10), &Tool::edit(Some("e1"), &["src/a.rs"]));
    b.tool_failed(
        s,
        at(11),
        &Tool::edit(Some("e1"), &["src/a.rs"]),
        "string_mismatch",
    );
    b.tool_start(s, at(20), &Tool::edit(Some("e2"), &["src/a.rs"]));
    b.tool_finish(
        s,
        at(21),
        &Tool::edit(Some("e2"), &["src/a.rs"]),
        Outcome::success(),
    );
    if late_first_failure {
        // A reconstructed event that arrives late and belongs earlier.
        b.tool_start(s, at(5), &Tool::edit(Some("e0"), &["src/z.rs"]));
        b.tool_failed(
            s,
            at(6),
            &Tool::edit(Some("e0"), &["src/z.rs"]),
            "string_mismatch",
        );
    }
    b.stop(s, at(30));
    b
}

fn retry_of(p: &Projection, s: &Sess) -> AttemptId {
    p.attempts_of(s.session_id)
        .find(|a| a.tool_call_ids.len() == 1 && a.outcome == AttemptOutcome::Succeeded)
        .expect("the retry")
        .attempt_id
}

#[test]
fn attempt_ids_survive_a_late_arriving_earlier_event() {
    let s = Sess::claude("stable");
    let before = project(&turn_with_two_edits(false, &s).build());
    let after = project(&turn_with_two_edits(true, &s).build());
    assert_eq!(before.attempts.len(), 2);
    assert_eq!(after.attempts.len(), 3, "the late failure is a new attempt");
    for a in &before.attempts {
        let same = after
            .attempts
            .iter()
            .find(|x| x.attempt_id == a.attempt_id)
            .unwrap_or_else(|| panic!("attempt {} changed id", a.attempt_id));
        assert_eq!(same.tool_call_ids, a.tool_call_ids);
        assert_eq!(same.paths, a.paths);
    }
    assert_eq!(retry_of(&before, &s), retry_of(&after, &s));
}

#[test]
fn a_correction_lands_on_the_attempt_it_named_even_after_a_late_event() {
    let s = Sess::claude("correction");
    let before = project(&turn_with_two_edits(false, &s).build());
    let target = retry_of(&before, &s);

    let mut b = turn_with_two_edits(true, &s);
    b.correction(
        &s,
        at(100),
        "attempt_outcome",
        &format!("att_{target}"),
        Some("failed"),
        None,
        Some("retry did not actually work"),
    );
    let p = project(&b.build());
    assert_eq!(p.corrections[0].status, CorrectionStatus::Applied);
    assert!(!p.corrections[0].legacy_position);
    let hit: Vec<_> = p
        .attempts
        .iter()
        .filter(|a| a.corrected.is_some())
        .collect();
    assert_eq!(hit.len(), 1);
    assert_eq!(
        hit[0].attempt_id, target,
        "the retry, not the early failure"
    );
    assert_eq!(hit[0].paths, vec!["src/a.rs".to_string()]);
}

#[test]
fn a_retraction_hits_the_same_attempt_after_a_late_event() {
    let s = Sess::claude("retraction");
    let before = project(&turn_with_two_edits(false, &s).build());
    let first_failure = before
        .attempts
        .iter()
        .find(|a| a.outcome == AttemptOutcome::Superseded)
        .unwrap()
        .attempt_id;
    let mut b = turn_with_two_edits(true, &s);
    b.retraction(
        &s,
        at(100),
        "attempt",
        &format!("att_{first_failure}"),
        "mistaken_import",
        None,
    );
    let p = project(&b.build());
    assert_eq!(p.retracted.attempts.len(), 1);
    assert_eq!(p.retracted.attempts[0].attempt_id, first_failure);
    assert_eq!(p.retracted.attempts[0].paths, vec!["src/a.rs".to_string()]);
    // The late failure on z.rs is untouched.
    assert!(
        p.attempts
            .iter()
            .any(|a| a.paths == vec!["src/z.rs".to_string()])
    );
}

#[test]
fn a_scoped_view_of_the_log_derives_the_same_ids() {
    // Two turns; a `--since` view that only holds the second one.
    let s = Sess::claude("scoped");
    let mut b = Stream::new();
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "first");
    edit_ok(&mut b, &s, 5, "e1", "src/a.rs");
    b.stop(&s, at(10));
    b.prompt(&s, at(100), "second");
    b.tool_start(&s, at(105), &Tool::edit(Some("e2"), &["src/b.rs"]));
    b.tool_failed(
        &s,
        at(106),
        &Tool::edit(Some("e2"), &["src/b.rs"]),
        "string_mismatch",
    );
    edit_ok(&mut b, &s, 110, "e3", "src/b.rs");
    b.stop(&s, at(120));
    let whole = project(&b.events);
    let scoped_events: Vec<Event> = b
        .events
        .iter()
        .filter(|e| e.observed_at >= at(100))
        .cloned()
        .collect();
    let scoped = project(&scoped_events);
    assert!(scoped.attempts.len() >= 2);
    for a in &scoped.attempts {
        assert!(
            whole.attempts.iter().any(|w| w.attempt_id == a.attempt_id),
            "scoped attempt {:?} has no counterpart in the whole log",
            a.paths
        );
    }
}

#[test]
fn positional_ids_from_before_tier1_v5_still_resolve_and_say_so() {
    let s = Sess::claude("legacy");
    let b0 = turn_with_two_edits(false, &s);
    let base = project(&b0.events);
    let retry = base
        .attempts
        .iter()
        .find(|a| a.outcome == AttemptOutcome::Succeeded)
        .unwrap();
    let legacy = retry.legacy_position_id();
    assert_ne!(legacy, retry.attempt_id);

    let mut b = turn_with_two_edits(false, &s);
    b.correction(
        &s,
        at(100),
        "attempt_outcome",
        &format!("att_{legacy}"),
        Some("failed"),
        None,
        None,
    );
    let p = project(&b.build());
    assert_eq!(p.corrections[0].status, CorrectionStatus::Applied);
    assert!(p.corrections[0].legacy_position, "resolved by position");
    let corrected = p.attempts.iter().find(|a| a.corrected.is_some()).unwrap();
    assert_eq!(corrected.attempt_id, retry.attempt_id);

    let mut b = turn_with_two_edits(false, &s);
    b.retraction(
        &s,
        at(100),
        "attempt",
        &format!("att_{legacy}"),
        "test",
        None,
    );
    let p = project(&b.build());
    assert_eq!(p.retracted.attempts[0].attempt_id, retry.attempt_id);
    assert!(p.retractions[0].matched);
    assert!(p.retractions[0].legacy_position);
    // Retracted by its own id, the flag stays off.
    let mut b = turn_with_two_edits(false, &s);
    b.retraction(
        &s,
        at(100),
        "attempt",
        &format!("att_{}", retry.attempt_id),
        "test",
        None,
    );
    let p = project(&b.build());
    assert!(!p.retractions[0].legacy_position);
}

#[test]
fn a_privacy_retraction_takes_the_prompt_text_with_it() {
    let s = Sess::claude("secret");
    let mut b = Stream::new();
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "SECRET: rotate the prod db password");
    edit_ok(&mut b, &s, 10, "e", "src/secret.rs");
    b.stop(&s, at(20));
    let base = project(&b.events);
    assert!(
        base.work_units[0]
            .objective
            .as_deref()
            .is_some_and(|o| o.starts_with("SECRET"))
    );
    let att = base.attempts[0].attempt_id;

    b.retraction(
        &s,
        at(100),
        "attempt",
        &format!("att_{att}"),
        "privacy",
        Some("SECRET note"),
    );
    b.correction(
        &s,
        at(101),
        "attempt_note",
        &format!("att_{att}"),
        None,
        None,
        Some("SECRET correction text"),
    );
    let p = project(&b.events);
    let text = serde_json::to_string(&p).unwrap();
    assert!(
        !text.contains("rotate the prod db"),
        "the prompt survives in the projection: {text}"
    );
    assert!(!text.contains("SECRET correction text"));
    assert!(p.turns.iter().all(|t| t.objective.is_none()));
    assert!(p.work_units.iter().all(|u| u.objective.is_none()));
    assert!(p.retracted.attempts.iter().all(|a| a.objective.is_none()));
    assert_eq!(p.corrections[0].status, CorrectionStatus::TargetRetracted);

    // Any other reason leaves the prompt alone.
    let mut b = Stream::new();
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "SECRET: rotate the prod db password");
    edit_ok(&mut b, &s, 10, "e", "src/secret.rs");
    b.stop(&s, at(20));
    b.retraction(&s, at(100), "attempt", &format!("att_{att}"), "test", None);
    let p = project(&b.events);
    assert!(
        serde_json::to_string(&p)
            .unwrap()
            .contains("rotate the prod db")
    );
}

#[test]
fn what_cites_a_retracted_attempt_goes_with_it() {
    // Claude edits a shared file; Codex takes over; the Claude attempt is
    // retracted. The handoff and every edge and unit must stop citing it.
    let a = Sess::claude("a");
    let c = Sess::codex("c");
    let mut b = Stream::new();
    b.session_started(&a, at(0));
    b.prompt(&a, at(1), "x");
    edit_ok(&mut b, &a, 10, "e", "src/shared.rs");
    b.stop(&a, at(20));
    b.session_ended(&a, at(25), "other");
    b.session_started(&c, at(120));
    b.prompt(&c, at(121), "y");
    edit_ok(&mut b, &c, 130, "e2", "src/shared.rs");
    b.stop(&c, at(140));
    let base = project(&b.events);
    assert_eq!(base.handoffs.len(), 1);
    let att = base
        .attempts
        .iter()
        .find(|x| x.session_id == a.session_id)
        .unwrap()
        .attempt_id;

    b.retraction(&a, at(500), "attempt", &format!("att_{att}"), "test", None);
    let p = project(&b.events);
    let cites_retracted = |ev: &[EventId]| ev.iter().any(|e| p.retracted_ids.contains_event(e));
    assert!(
        p.handoffs.is_empty(),
        "the handoff rested on retracted events"
    );
    assert!(p.edges.iter().all(|e| !cites_retracted(&e.evidence)));
    assert!(p.work_units.iter().all(|u| !cites_retracted(&u.evidence)));
    assert!(
        p.attempts
            .iter()
            .all(|x| !cites_retracted(&x.evidence) || x.session_id == a.session_id)
    );
    for t in &p.turns {
        assert!(!p.retracted_ids.contains_event(&t.first_event_id));
        assert!(!p.retracted_ids.contains_event(&t.last_event_id));
    }
}

#[test]
fn a_correction_with_no_stored_text_is_not_reported_applied() {
    let s = Sess::claude("meta-only");
    let mut b = Stream::metadata_only();
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "fix");
    edit_ok(&mut b, &s, 10, "e", "src/a.rs");
    b.stop(&s, at(20));
    let base = project(&b.events);
    let turn = base.turns[0].turn_id;
    let att = base.attempts[0].attempt_id;
    b.correction(
        &s,
        at(100),
        "turn_objective",
        &format!("trn_{turn}"),
        None,
        None,
        Some("new objective"),
    );
    b.correction(
        &s,
        at(101),
        "attempt_note",
        &format!("att_{att}"),
        None,
        None,
        Some("a note"),
    );
    b.correction(
        &s,
        at(102),
        "attempt_outcome",
        &format!("att_{att}"),
        Some("failed"),
        None,
        Some("why"),
    );
    let p = project(&b.events);
    let statuses: Vec<_> = p.corrections.iter().map(|c| c.status).collect();
    assert_eq!(
        statuses,
        vec![
            CorrectionStatus::ContentUnavailable,
            CorrectionStatus::ContentUnavailable,
            CorrectionStatus::Applied,
        ],
        "the text was not stored; only the outcome could be applied"
    );
    assert!(p.turns[0].corrected.is_none(), "the turn is untouched");
    assert_eq!(p.stats.corrections_applied, 1);
}

#[test]
fn a_correction_written_later_does_not_change_the_past() {
    let s = Sess::claude("time-travel");
    let mut b = Stream::new();
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "x");
    b.tool_start(&s, at(10), &Tool::edit(Some("e1"), &["src/a.rs"]));
    b.tool_failed(
        &s,
        at(11),
        &Tool::edit(Some("e1"), &["src/a.rs"]),
        "string_mismatch",
    );
    b.tool_start(&s, at(20), &Tool::edit(Some("e2"), &["src/b.rs"]));
    b.tool_failed(
        &s,
        at(21),
        &Tool::edit(Some("e2"), &["src/b.rs"]),
        "string_mismatch",
    );
    b.stop(&s, at(30));
    let p = project(&b.events);
    let last = p.attempts.last().unwrap().attempt_id;
    assert!(p.state_at(at(40)).sessions[0].blocked, "two failures alike");

    b.correction(
        &s,
        at(3_600),
        "attempt_outcome",
        &format!("att_{last}"),
        Some("succeeded"),
        None,
        None,
    );
    let p = project(&b.events);
    let st = &p.state_at(at(40)).sessions[0];
    assert!(st.blocked, "as known at t=40 the session was blocked");
    assert_eq!(st.last_attempt_outcome, Some(AttemptOutcome::Failed));
    let later = &p.state_at(at(3_700)).sessions[0];
    assert!(!later.blocked, "after the correction it is not");
}

#[test]
fn an_event_pushed_twice_is_one_event() {
    let s = Sess::claude("dup");
    let mut b = Stream::new();
    b.session_started(&s, at(0));
    b.prompt(&s, at(1), "x");
    edit_ok(&mut b, &s, 10, "e", "src/a.rs");
    b.stop(&s, at(20));
    let once = project(&b.events);

    let mut p = Projector::new();
    for ev in b.events.iter().chain(b.events.iter()) {
        p.push(ev);
    }
    let twice = p.finish();
    assert_eq!(twice.stats.events_seen, b.events.len() as u64);
    assert_eq!(twice.sessions[0].event_count, once.sessions[0].event_count);
    assert_eq!(twice, once);
}

#[test]
fn sessions_with_no_provider_session_id_do_not_merge_projects() {
    let mut events = Vec::new();
    for (root, secs) in [("/work/one", 0), ("/work/two", 100)] {
        let device = attemptdb_core::DeviceId::derive(&["d"]);
        let project = attemptdb_core::ProjectRef::derive(root, None, &device);
        let mut ev = Event::new(
            device,
            attemptdb_core::event::Provider::ClaudeCode,
            "UserPromptSubmit",
            attemptdb_core::EventKind::PromptSubmitted,
            project,
            "unknown",
            attemptdb_core::CaptureMode::LocalSemantic,
            "test",
        );
        ev.event_id = EventId::derive(&["unknown", root]);
        ev.observed_at = at(secs);
        ev.captured_at = ev.observed_at;
        events.push(ev);
    }
    let p = project(&events);
    assert_eq!(p.sessions.len(), 2, "one session per project");
    assert_ne!(p.sessions[0].project_id, p.sessions[1].project_id);
    assert!(
        p.sessions
            .iter()
            .all(|s| s.provider_session_id == "unknown")
    );
}
