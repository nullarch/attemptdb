//! The Needs You queue: what enters it, in what order, and — the product
//! requirement that matters most — what must never enter it.

mod common;

use attemptdb_core::{Outcome, Timestamp};
use attemptdb_project::{AttentionKind, DEFAULT_MIN_CONFIDENCE, project};
use common::{Sess, Stream, Tool, at};

fn queue(st: Stream, now_secs: i64) -> Vec<attemptdb_project::AttentionItem> {
    let p = project(&st.build());
    p.attention_at(at(now_secs), DEFAULT_MIN_CONFIDENCE)
}

/// A complete, ordinary session: prompt, edit, test, stop, session end.
fn clean_session() -> Stream {
    let s = Sess::claude("clean");
    let mut st = Stream::new();
    let f = ["src/lib.rs"];
    st.session_started(&s, at(0));
    st.prompt(&s, at(1), "add the missing test");
    st.tool_start(&s, at(2), &Tool::edit(Some("c1"), &f));
    st.tool_finish(&s, at(3), &Tool::edit(Some("c1"), &f), Outcome::success());
    st.tool_start(&s, at(4), &Tool::shell(Some("c2")));
    st.tool_finish(&s, at(9), &Tool::shell(Some("c2")), Outcome::success());
    st.stop(&s, at(10));
    st
}

#[test]
fn a_normal_completed_turn_is_not_attention() {
    let mut st = clean_session();
    let s = Sess::claude("clean");
    st.session_ended(&s, at(11), "exit");
    assert!(queue(st, 3_600).is_empty());
}

#[test]
fn an_idle_open_session_is_not_attention() {
    // Open, quiet for an hour, but nothing ever asked for a human.
    assert!(queue(clean_session(), 3_600).is_empty());
}

#[test]
fn a_single_failure_is_not_attention() {
    let s = Sess::claude("one-failure");
    let mut st = Stream::new();
    let f = ["src/parser.rs"];
    st.session_started(&s, at(0));
    st.prompt(&s, at(1), "fix the parser");
    st.tool_start(&s, at(2), &Tool::edit(Some("c1"), &f));
    st.tool_failed(&s, at(3), &Tool::edit(Some("c1"), &f), "string_mismatch");
    st.stop(&s, at(4));
    assert!(queue(st, 600).is_empty());
}

#[test]
fn a_permission_request_ranks_first_and_reports_the_wait() {
    let s = Sess::claude("gate");
    let mut st = Stream::new();
    st.session_started(&s, at(0));
    st.prompt(&s, at(1), "run the migration");
    st.permission_requested(&s, at(30), &Tool::shell(Some("c1")));
    let items = queue(st, 30 + 900);
    assert_eq!(items.len(), 1);
    let it = &items[0];
    assert_eq!(it.kind, AttentionKind::PermissionGate);
    assert_eq!(it.rank, 1);
    assert_eq!(it.signal_type.as_deref(), Some("permission_request"));
    assert_eq!(it.waiting_ms, 900_000);
    assert!(it.action.starts_with("Approve or deny"), "{}", it.action);
    assert_eq!(it.evidence.len(), 1);
    assert!(it.confidence >= 0.6);
    assert!(!it.uncertainty.is_empty());
}

#[test]
fn a_cleared_signal_leaves_the_queue() {
    let s = Sess::claude("cleared");
    let mut st = Stream::new();
    let f = ["src/lib.rs"];
    st.session_started(&s, at(0));
    st.prompt(&s, at(1), "run the migration");
    st.permission_requested(&s, at(30), &Tool::shell(Some("c1")));
    // The human answered: a later event exists in the session.
    st.tool_start(&s, at(60), &Tool::edit(Some("c2"), &f));
    st.tool_finish(&s, at(61), &Tool::edit(Some("c2"), &f), Outcome::success());
    assert!(queue(st, 900).is_empty());
}

#[test]
fn a_signal_in_an_ended_session_is_not_attention() {
    let s = Sess::claude("gone");
    let mut st = Stream::new();
    st.session_started(&s, at(0));
    st.prompt(&s, at(1), "run the migration");
    st.permission_requested(&s, at(30), &Tool::shell(Some("c1")));
    st.session_ended(&s, at(40), "exit");
    assert!(queue(st, 900).is_empty());
}

#[test]
fn an_idle_prompt_notification_is_an_input_request() {
    let s = Sess::claude("asking");
    let mut st = Stream::new();
    st.session_started(&s, at(0));
    st.prompt(&s, at(1), "pick a database");
    st.notification(&s, at(20), "agent_needs_input");
    let items = queue(st, 620);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].kind, AttentionKind::InputRequest);
    assert_eq!(items[0].rank, 2);
    assert_eq!(items[0].signal_type.as_deref(), Some("agent_needs_input"));
    assert!(
        items[0].action.starts_with("Answer the"),
        "{}",
        items[0].action
    );
}

/// Two attempts failing the same way, nothing successful after them.
fn repeating(same_class: bool) -> Stream {
    let s = Sess::claude("looping");
    let mut st = Stream::new();
    let f = ["src/parser.rs"];
    st.session_started(&s, at(0));
    st.prompt(&s, at(1), "fix the parser");
    st.tool_start(&s, at(2), &Tool::edit(Some("c1"), &f));
    st.tool_failed(&s, at(3), &Tool::edit(Some("c1"), &f), "string_mismatch");
    st.tool_start(&s, at(20), &Tool::edit(Some("c2"), &f));
    st.tool_failed(
        &s,
        at(21),
        &Tool::edit(Some("c2"), &f),
        if same_class {
            "string_mismatch"
        } else {
            "permission_denied"
        },
    );
    st.stop(&s, at(22));
    st
}

#[test]
fn two_failures_of_the_same_class_reach_the_queue() {
    let items = queue(repeating(true), 300);
    assert_eq!(items.len(), 1, "{items:#?}");
    let it = &items[0];
    assert_eq!(it.kind, AttentionKind::RepeatedFailure);
    assert_eq!(it.rank, 3);
    assert_eq!(it.failure_class.as_deref(), Some("string_mismatch"));
    assert!(it.work_unit_id.is_some());
    assert!(it.evidence.len() >= 2);
    assert!(it.claim.contains("repeating itself"), "{}", it.claim);
}

#[test]
fn two_failures_of_different_classes_do_not() {
    assert!(queue(repeating(false), 300).is_empty());
}

#[test]
fn a_successful_retry_after_the_failures_clears_the_loop() {
    let s = Sess::claude("looping");
    let mut st = repeating(true);
    let f = ["src/parser.rs"];
    st.prompt(&s, at(40), "try a different approach");
    st.tool_start(&s, at(41), &Tool::edit(Some("c3"), &f));
    st.tool_finish(&s, at(42), &Tool::edit(Some("c3"), &f), Outcome::success());
    st.stop(&s, at(43));
    assert!(queue(st, 300).is_empty());
}

#[test]
fn a_gate_outranks_a_repeated_failure_in_the_same_session() {
    // The agent looped twice, then asked for permission: two items, the
    // gate first.
    let s = Sess::claude("looping");
    let mut st = repeating(true);
    st.permission_requested(&s, at(30), &Tool::shell(Some("c9")));
    let items = queue(st, 900);
    assert_eq!(items.len(), 2, "{items:#?}");
    assert_eq!(items[0].kind, AttentionKind::PermissionGate);
    assert_eq!(items[1].kind, AttentionKind::RepeatedFailure);
}

#[test]
fn within_a_rank_the_longest_wait_comes_first() {
    let early = Sess::codex("gate-early");
    let late = Sess::claude("gate-late");
    let mut st = Stream::new();
    st.session_started(&early, at(100));
    st.prompt(&early, at(101), "deploy");
    st.permission_requested(&early, at(110), &Tool::codex_shell(Some("x1")));
    st.session_started(&late, at(200));
    st.prompt(&late, at(201), "deploy");
    st.permission_requested(&late, at(210), &Tool::shell(Some("c9")));

    let items = queue(st, 900);
    assert_eq!(items.len(), 2, "{items:#?}");
    assert!(
        items
            .iter()
            .all(|i| i.kind == AttentionKind::PermissionGate)
    );
    assert!(items[0].since < items[1].since);
    assert!(items[0].waiting_ms > items[1].waiting_ms);
}

#[test]
fn attention_ids_are_stable_across_evaluations() {
    let st = repeating(true);
    let p = project(&st.build());
    let a = p.attention_at(at(300), DEFAULT_MIN_CONFIDENCE);
    let b = p.attention_at(at(400), DEFAULT_MIN_CONFIDENCE);
    assert_eq!(a[0].attention_id, b[0].attention_id);
    assert!(b[0].waiting_ms > a[0].waiting_ms);
}

#[test]
fn an_empty_projection_has_an_empty_queue() {
    let p = project(&[]);
    assert!(p.attention().is_empty());
    assert!(p.attention_at(Timestamp::now(), 0.0).is_empty());
}

/// Two agents editing one file in overlapping windows (the `conflicts.rs`
/// reference story) is the fourth and last thing worth interrupting for.
#[test]
fn a_work_conflict_is_the_lowest_ranked_item() {
    let a = Sess::claude("kevin-claude");
    let b = Sess::codex("sarah-codex");
    let shared = ["src/middleware/auth.ts"];
    let mut st = Stream::new();
    st.session_started(&a, at(0));
    st.prompt(&a, at(1), "tidy the auth middleware");
    for (i, t) in [(1, 10), (2, 100)] {
        let tool = Tool::edit(Some(if i == 1 { "a1" } else { "a2" }), &shared);
        st.tool_start(&a, at(t), &tool);
        st.tool_finish(&a, at(t + 1), &tool, Outcome::success());
    }
    st.session_started(&b, at(40));
    st.prompt(&b, at(41), "rename the session helper");
    for (i, t) in [(1, 50), (2, 120)] {
        let tool = Tool::edit(Some(if i == 1 { "b1" } else { "b2" }), &shared);
        st.tool_start(&b, at(t), &tool);
        st.tool_finish(&b, at(t + 1), &tool, Outcome::success());
    }
    let events = st.build();
    let p = project(&events);
    assert_eq!(p.conflicts.len(), 1, "the fixture must produce a conflict");
    let items = p.attention_at(at(600), DEFAULT_MIN_CONFIDENCE);
    let conflict: Vec<_> = items
        .iter()
        .filter(|i| i.kind == AttentionKind::WorkConflict)
        .collect();
    assert_eq!(conflict.len(), 1, "{items:#?}");
    let it = conflict[0];
    assert_eq!(it.rank, 4);
    assert!(
        it.action.starts_with("Reconcile two open work units"),
        "{}",
        it.action
    );
    assert!(it.claim.contains("shared path"), "{}", it.claim);
    assert!(!it.evidence.is_empty());
    // Every item is ordered after the higher ranks.
    assert_eq!(
        items.last().map(|i| i.kind),
        Some(AttentionKind::WorkConflict)
    );
}
