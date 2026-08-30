//! Artifact linkage: a successful `git commit` tool call is tied to the sha
//! the repository moved to, using only the `HEAD` the hook records on every
//! event — never command output.

mod common;

use attemptdb_core::{CommitId, Outcome};
use attemptdb_project::project;
use common::{Sess, Stream, at};

/// Set the git context on the most recently pushed event.
fn head(st: &mut Stream, sha: &str) {
    let ev = st.events.last_mut().expect("an event was pushed");
    ev.project.head = Some(sha.into());
    ev.project.branch = Some("main".into());
}

#[test]
fn commit_whose_end_event_carries_the_new_head_links_at_0_9() {
    let s = Sess::claude("s1");
    let mut st = Stream::new();
    st.session_started(&s, at(0));
    head(&mut st, "aaa");
    st.prompt(&s, at(1), "ship the settlement endpoint");
    head(&mut st, "aaa");
    let (start, end) = st.shell_classified(
        &s,
        at(2),
        at(3),
        "c1",
        "git",
        Some("commit"),
        Outcome::success(),
    );
    // PreToolUse still sees the old head; PostToolUse runs after git did.
    let n = st.events.len();
    st.events[n - 2].project.head = Some("aaa".into());
    st.events[n - 1].project.head = Some("bbb".into());
    st.events[n - 1].project.branch = Some("main".into());
    st.stop(&s, at(4));
    head(&mut st, "bbb");
    let events = st.build();

    let p = project(&events);
    assert_eq!(p.commits.len(), 1);
    let c = &p.commits[0];
    assert_eq!(c.sha.as_deref(), Some("bbb"));
    assert_eq!(c.previous_sha.as_deref(), Some("aaa"));
    assert_eq!(c.branch.as_deref(), Some("main"));
    assert_eq!(c.linkage, "end_event");
    assert!((c.confidence - 0.9).abs() < 1e-6);
    assert_eq!(c.evidence, vec![start, end]);
    assert_eq!(c.at, at(3));
    assert_eq!(c.session_id, s.session_id);

    let a = p.attempts_of(s.session_id).next().expect("one attempt");
    assert_eq!(c.attempt_id, Some(a.attempt_id));
    assert_eq!(c.turn_id, Some(a.turn_id));
    assert_eq!(a.commit_shas, vec!["bbb".to_string()]);
    assert_eq!(p.work_units.len(), 1);
    assert_eq!(p.work_units[0].commit_shas, vec!["bbb".to_string()]);

    // Deterministic id: the same stream projects to the same commit id.
    let again = project(&events);
    assert_eq!(again.commits[0].commit_id, c.commit_id);
    assert_eq!(
        c.commit_id,
        CommitId::derive(&[
            "session",
            &s.session_id.to_string(),
            "call",
            &c.tool_call_id.to_string()
        ])
    );
    let json = serde_json::to_string(&p).unwrap();
    assert!(json.contains("\"commits\":[{") && json.contains("\"commit_shas\":[\"bbb\"]"));
}

#[test]
fn commit_resolved_by_the_next_head_bearing_event_links_at_0_7() {
    let s = Sess::claude("s2");
    let mut st = Stream::new();
    st.session_started(&s, at(0));
    head(&mut st, "aaa");
    st.prompt(&s, at(1), "commit");
    let (start, end) = st.shell_classified(
        &s,
        at(2),
        at(3),
        "c1",
        "git",
        Some("commit"),
        Outcome::success(),
    );
    // Neither call event carries git context (e.g. an adapter without it);
    // the stop event does.
    let stop = st.stop(&s, at(4));
    head(&mut st, "bbb");

    let p = project(&st.build());
    assert_eq!(p.commits.len(), 1);
    let c = &p.commits[0];
    assert_eq!(c.sha.as_deref(), Some("bbb"));
    assert_eq!(c.previous_sha.as_deref(), Some("aaa"));
    assert_eq!(c.linkage, "next_head");
    assert!((c.confidence - 0.7).abs() < 1e-6);
    assert_eq!(c.evidence, vec![start, end, stop]);
}

#[test]
fn unresolved_and_failed_commits() {
    let s = Sess::claude("s3");
    let mut st = Stream::new();
    st.session_started(&s, at(0));
    head(&mut st, "aaa");
    st.prompt(&s, at(1), "try to commit");
    // A commit that failed (pre-commit hook) is not a commit.
    st.shell_classified(
        &s,
        at(2),
        at(3),
        "c1",
        "git",
        Some("commit"),
        Outcome::failure(Some("exit_code".into())),
    );
    head(&mut st, "aaa");
    // A successful one whose head never showed up afterwards.
    let (_, end) = st.shell_classified(
        &s,
        at(4),
        at(5),
        "c2",
        "git",
        Some("commit"),
        Outcome::success(),
    );
    // `git push` moves nothing locally and is not a commit.
    st.shell_classified(
        &s,
        at(6),
        at(7),
        "c3",
        "git",
        Some("push"),
        Outcome::success(),
    );

    let p = project(&st.build());
    assert_eq!(p.commits.len(), 1, "only the successful commit call");
    let c = &p.commits[0];
    assert_eq!(c.sha, None);
    assert_eq!(c.linkage, "unresolved");
    assert!((c.confidence - 0.4).abs() < 1e-6);
    assert!(c.evidence.contains(&end));
    let a = p.attempts_of(s.session_id).next().unwrap();
    assert!(a.commit_shas.is_empty(), "no sha, nothing to claim");
}

#[test]
fn consecutive_commits_consume_head_changes_in_order() {
    let s = Sess::claude("s4");
    let mut st = Stream::new();
    st.session_started(&s, at(0));
    head(&mut st, "aaa");
    st.prompt(&s, at(1), "two commits");
    st.shell_classified(
        &s,
        at(2),
        at(3),
        "c1",
        "git",
        Some("commit"),
        Outcome::success(),
    );
    st.agent_message(&s, at(4));
    head(&mut st, "bbb");
    st.shell_classified(
        &s,
        at(5),
        at(6),
        "c2",
        "git",
        Some("commit"),
        Outcome::success(),
    );
    let n = st.events.len();
    st.events[n - 2].project.head = Some("bbb".into());
    st.stop(&s, at(7));
    head(&mut st, "ccc");

    let p = project(&st.build());
    let shas: Vec<Option<&str>> = p.commits.iter().map(|c| c.sha.as_deref()).collect();
    assert_eq!(shas, vec![Some("bbb"), Some("ccc")]);
    assert_eq!(p.commits[0].previous_sha.as_deref(), Some("aaa"));
    assert_eq!(p.commits[1].previous_sha.as_deref(), Some("bbb"));
    let a = p.attempts_of(s.session_id).next().unwrap();
    assert_eq!(a.commit_shas, vec!["bbb".to_string(), "ccc".to_string()]);
}
