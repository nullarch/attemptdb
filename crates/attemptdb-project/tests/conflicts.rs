//! Work conflicts (`conflict-v0`): two agents editing the same file in
//! overlapping windows, neither committed since.

mod common;

use attemptdb_core::Outcome;
use attemptdb_project::project;
use common::{Sess, Stream, Tool, at};

/// Two sessions from different agents; each edits `paths` in its own turn.
fn two_units(
    a_paths: &[&str],
    b_paths: &[&str],
    a_edit_at: (i64, i64),
    b_edit_at: (i64, i64),
) -> Stream {
    let a = Sess::claude("kevin-claude");
    let b = Sess::codex("sarah-codex");
    let mut st = Stream::new();
    st.session_started(&a, at(0));
    st.prompt(&a, at(1), "tidy the auth middleware");
    st.tool_start(&a, at(a_edit_at.0), &Tool::edit(Some("a1"), a_paths));
    st.tool_finish(
        &a,
        at(a_edit_at.0 + 1),
        &Tool::edit(Some("a1"), a_paths),
        Outcome::success(),
    );
    st.events
        .last_mut()
        .unwrap()
        .attrs
        .insert("lines_added".into(), 88.into());
    st.events
        .last_mut()
        .unwrap()
        .attrs
        .insert("lines_removed".into(), 12.into());
    st.tool_start(&a, at(a_edit_at.1), &Tool::edit(Some("a2"), a_paths));
    st.tool_finish(
        &a,
        at(a_edit_at.1 + 1),
        &Tool::edit(Some("a2"), a_paths),
        Outcome::success(),
    );

    st.session_started(&b, at(b_edit_at.0 - 5));
    st.prompt(&b, at(b_edit_at.0 - 4), "fix session expiry handling");
    st.tool_start(&b, at(b_edit_at.0), &Tool::edit(Some("b1"), b_paths));
    st.tool_finish(
        &b,
        at(b_edit_at.0 + 1),
        &Tool::edit(Some("b1"), b_paths),
        Outcome::success(),
    );
    st.events
        .last_mut()
        .unwrap()
        .attrs
        .insert("lines_added".into(), 41.into());
    st.tool_start(&b, at(b_edit_at.1), &Tool::edit(Some("b2"), b_paths));
    st.tool_finish(
        &b,
        at(b_edit_at.1 + 1),
        &Tool::edit(Some("b2"), b_paths),
        Outcome::success(),
    );
    st
}

#[test]
fn overlapping_edits_of_one_file_by_two_agents_are_a_conflict() {
    let st = two_units(
        &["src/middleware/auth.ts", "tests/auth.spec.ts"],
        &["src/middleware/auth.ts", "src/middleware/session.ts"],
        (10, 100),
        (50, 120),
    );
    let events = st.build();
    let p = project(&events);
    assert_eq!(p.work_units.len(), 2, "one unit per agent");
    assert_eq!(p.conflicts.len(), 1, "{:?}", p.conflicts);
    let c = &p.conflicts[0];
    assert_eq!(c.algorithm_version, "conflict-v0");
    assert!(c.first_started_at <= c.second_started_at);
    assert_eq!(c.paths.len(), 1, "only the shared path: {:?}", c.paths);
    let path = &c.paths[0];
    assert!(path.path.ends_with("src/middleware/auth.ts"));
    assert!(path.overlapping);
    assert_eq!((path.first_added, path.first_removed), (88, 12));
    assert_eq!((path.second_added, path.second_removed), (41, 0));
    assert!(!path.first_committed && !path.second_committed);
    assert!((c.confidence - 0.7).abs() < 1e-6);
    assert!(!c.evidence.is_empty(), "the edit events are the evidence");
    assert_eq!(c.started_at, at(11), "first edit among the shared path");
    assert_eq!(c.updated_at, at(121));
    // It is a table too.
    assert_eq!(p.conflicts[0].project_id, p.work_units[0].project_id);
}

#[test]
fn no_conflict_without_a_shared_file_or_outside_the_window() {
    // Different files.
    let p = project(&two_units(&["a.ts"], &["b.ts"], (10, 100), (50, 120)).build());
    assert!(p.conflicts.is_empty());
    // Same file, a day apart.
    let p = project(&two_units(&["a.ts"], &["a.ts"], (10, 100), (90_000, 90_100)).build());
    assert!(p.conflicts.is_empty());
    // Same file, an hour apart, sessions in sequence: that is one piece of
    // work continued (rule 1 links them), so no conflict and one unit.
    let p = project(&two_units(&["a.ts"], &["a.ts"], (10, 100), (3_700, 3_800)).build());
    assert_eq!(p.work_units.len(), 1);
    assert!(p.conflicts.is_empty());
}

#[test]
fn concurrent_sessions_whose_edits_are_an_hour_apart_conflict_at_lower_confidence() {
    // Sarah's session is open the whole time (her prompt lands before
    // Kevin edits) but her edits come an hour after his: separate units,
    // edit windows within the two-hour window, no overlap.
    let a = Sess::claude("kevin-claude");
    let b = Sess::codex("sarah-codex");
    let mut st = Stream::new();
    st.session_started(&b, at(0));
    st.prompt(&b, at(1), "fix expiry");
    st.tool_start(&b, at(2), &Tool::read(Some("r"), &["auth.ts"]));
    st.tool_finish(
        &b,
        at(3),
        &Tool::read(Some("r"), &["auth.ts"]),
        Outcome::success(),
    );
    st.session_started(&a, at(5));
    st.prompt(&a, at(6), "tidy auth");
    st.tool_start(&a, at(10), &Tool::edit(Some("a1"), &["auth.ts"]));
    st.tool_finish(
        &a,
        at(11),
        &Tool::edit(Some("a1"), &["auth.ts"]),
        Outcome::success(),
    );
    st.tool_start(&b, at(3_700), &Tool::edit(Some("b1"), &["auth.ts"]));
    st.tool_finish(
        &b,
        at(3_701),
        &Tool::edit(Some("b1"), &["auth.ts"]),
        Outcome::success(),
    );
    let p = project(&st.build());
    assert_eq!(
        p.work_units.len(),
        2,
        "{:?}",
        p.work_units.iter().map(|u| &u.sessions).collect::<Vec<_>>()
    );
    assert_eq!(p.conflicts.len(), 1);
    assert!(!p.conflicts[0].paths[0].overlapping);
    assert!((p.conflicts[0].confidence - 0.5).abs() < 1e-6);
}

#[test]
fn a_commit_after_the_last_edit_lowers_the_confidence() {
    let a = Sess::claude("kevin-claude");
    let b = Sess::codex("sarah-codex");
    let mut st = Stream::new();
    st.session_started(&a, at(0));
    st.prompt(&a, at(1), "tidy auth");
    st.tool_start(&a, at(10), &Tool::edit(Some("a1"), &["auth.ts"]));
    st.tool_finish(
        &a,
        at(11),
        &Tool::edit(Some("a1"), &["auth.ts"]),
        Outcome::success(),
    );
    // Kevin commits right after editing.
    let n0 = st.events.len();
    st.shell_classified(
        &a,
        at(20),
        at(21),
        "c1",
        "git",
        Some("commit"),
        Outcome::success(),
    );
    let n = st.events.len();
    assert!(n > n0);
    st.events[n - 2].project.head = Some("aaa".into());
    st.events[n - 1].project.head = Some("bbb".into());
    st.session_started(&b, at(5));
    st.prompt(&b, at(6), "fix expiry");
    st.tool_start(&b, at(15), &Tool::edit(Some("b1"), &["auth.ts"]));
    st.tool_finish(
        &b,
        at(16),
        &Tool::edit(Some("b1"), &["auth.ts"]),
        Outcome::success(),
    );
    let p = project(&st.build());
    assert_eq!(p.conflicts.len(), 1, "{:?}", p.conflicts);
    let c = &p.conflicts[0];
    assert!(c.paths[0].first_committed, "{:?}", c.paths);
    assert!(!c.paths[0].second_committed);
    assert!((c.confidence - 0.5).abs() < 1e-6);
}
