//! What belongs together: work units link through edited paths within a
//! bounded gap and never through hot files; handoffs need an edited path
//! (or a very short gap), survive a round trip between agents, and conflicts
//! are keyed by the repository-relative path.

mod common;

use attemptdb_core::{Event, Outcome, ToolCategory};
use attemptdb_project::{HOT_PATH_MIN_SESSIONS, PATH_LINK_GAP_US, project};
use common::{Sess, Stream, Tool, at};

const HOUR: i64 = 3_600;
const DAY: i64 = 86_400;

fn edit_turn(b: &mut Stream, s: &Sess, t: i64, path: &str, call: &str, prompt: &str) {
    let paths = [path];
    b.prompt(s, at(t), prompt);
    b.tool_start(s, at(t + 5), &Tool::edit(Some(call), &paths));
    b.tool_finish(
        s,
        at(t + 6),
        &Tool::edit(Some(call), &paths),
        Outcome::success(),
    );
    b.stop(s, at(t + 10));
}

#[test]
fn a_changelog_touched_by_every_session_does_not_fuse_them_into_one_unit() {
    let mut b = Stream::new();
    for d in 0..20 {
        let s = Sess::claude(&format!("s{d}"));
        let own = format!("src/f{d}.rs");
        let t0 = d * DAY;
        b.session_started(&s, at(t0));
        b.prompt(&s, at(t0 + 5), &format!("task {d}"));
        b.tool_start(&s, at(t0 + 10), &Tool::edit(Some("a"), &[own.as_str()]));
        b.tool_finish(
            &s,
            at(t0 + 11),
            &Tool::edit(Some("a"), &[own.as_str()]),
            Outcome::success(),
        );
        b.tool_start(&s, at(t0 + 12), &Tool::edit(Some("b"), &["CHANGELOG.md"]));
        b.tool_finish(
            &s,
            at(t0 + 13),
            &Tool::edit(Some("b"), &["CHANGELOG.md"]),
            Outcome::success(),
        );
        b.stop(&s, at(t0 + 20));
        b.session_ended(&s, at(t0 + 30), "other");
    }
    let p = project(&b.build());
    assert_eq!(
        p.work_units.len(),
        20,
        "one unit per task, not one for 20 days"
    );
    assert!(p.work_units.iter().all(|u| u.sessions.len() == 1));

    // The same changelog in a project of a few sessions in a row is
    // continuity: below the hot-file threshold it still links.
    let mut b = Stream::new();
    for d in 0..2 {
        let s = Sess::claude(&format!("few{d}"));
        b.session_started(&s, at(d * 600));
        edit_turn(
            &mut b,
            &s,
            d * 600 + 5,
            "CHANGELOG.md",
            "c",
            "update the changelog",
        );
        b.session_ended(&s, at(d * 600 + 100), "other");
    }
    assert!(2 < HOT_PATH_MIN_SESSIONS);
    let p = project(&b.build());
    assert_eq!(
        p.work_units.len(),
        1,
        "a short sequence on one file is one thread"
    );
}

#[test]
fn a_path_links_turns_only_within_the_gap_and_only_when_edited() {
    let s = Sess::claude("gap");
    let mut b = Stream::new();
    b.session_started(&s, at(0));
    edit_turn(&mut b, &s, 5, "src/a.rs", "e1", "first");
    // Two hours later: the same thread.
    edit_turn(&mut b, &s, 2 * HOUR, "src/a.rs", "e2", "second");
    // Far beyond the gap: a new piece of work on the same file.
    let far = 2 * HOUR + PATH_LINK_GAP_US / 1_000_000 + HOUR;
    edit_turn(&mut b, &s, far, "src/a.rs", "e3", "third");
    let p = project(&b.build());
    assert_eq!(p.work_units.len(), 2, "{:#?}", p.work_units);
    let sizes: Vec<usize> = p.work_units.iter().map(|u| u.turns.len()).collect();
    assert_eq!(sizes, vec![2, 1]);

    // A path another session only *read* links nothing, and neither does a
    // path a shell command merely mentioned.
    let a = Sess::claude("reader-a");
    let c = Sess::codex("reader-b");
    let mut b = Stream::new();
    b.session_started(&a, at(0));
    edit_turn(&mut b, &a, 5, "src/a.rs", "e1", "edit it");
    b.session_started(&c, at(HOUR));
    b.prompt(&c, at(HOUR + 5), "read it");
    b.tool_start(&c, at(HOUR + 6), &Tool::read(Some("r"), &["src/a.rs"]));
    b.tool_finish(
        &c,
        at(HOUR + 7),
        &Tool::read(Some("r"), &["src/a.rs"]),
        Outcome::success(),
    );
    let shell = Tool {
        name: "Bash",
        category: ToolCategory::Shell,
        call_id: Some("sh"),
        paths: &["src/a.rs"],
    };
    b.tool_start(&c, at(HOUR + 8), &shell);
    b.tool_finish(&c, at(HOUR + 9), &shell, Outcome::success());
    b.stop(&c, at(HOUR + 10));
    let p = project(&b.build());
    assert_eq!(
        p.work_units.len(),
        2,
        "reading or mentioning a file is not editing it"
    );
}

#[test]
fn a_later_turn_on_a_shared_file_does_not_fuse_two_concurrent_units() {
    let a = Sess::claude("a");
    let c = Sess::codex("c");
    let mut b = Stream::new();
    b.session_started(&a, at(0));
    b.prompt(&a, at(1), "add feature X");
    b.session_started(&c, at(2));
    b.prompt(&c, at(3), "fix bug Y");
    for i in 0..3 {
        let t = 10 + i * 100;
        let (ca, cc) = (format!("a{i}"), format!("c{i}"));
        b.tool_start(&a, at(t), &Tool::edit(Some(&ca), &["src/lib.rs"]));
        b.tool_finish(
            &a,
            at(t + 1),
            &Tool::edit(Some(&ca), &["src/lib.rs"]),
            Outcome::success(),
        );
        b.tool_start(
            &c,
            at(t + 50),
            &Tool::apply_patch(Some(&cc), &["src/lib.rs"]),
        );
        b.tool_finish(
            &c,
            at(t + 51),
            &Tool::apply_patch(Some(&cc), &["src/lib.rs"]),
            Outcome::success(),
        );
    }
    b.stop(&a, at(400));
    b.stop(&c, at(410));
    // The next day Claude carries on in the same session.
    edit_turn(&mut b, &a, DAY, "src/lib.rs", "n", "continue feature X");
    let p = project(&b.build());
    assert_eq!(
        p.work_units.len(),
        3,
        "one-day-old work is not a continuation"
    );

    // An hour later it is: it joins its own session's unit, not Codex's.
    let mut b = Stream::new();
    b.session_started(&a, at(0));
    b.prompt(&a, at(1), "add feature X");
    b.session_started(&c, at(2));
    b.prompt(&c, at(3), "fix bug Y");
    b.tool_start(&a, at(10), &Tool::edit(Some("a0"), &["src/lib.rs"]));
    b.tool_finish(
        &a,
        at(11),
        &Tool::edit(Some("a0"), &["src/lib.rs"]),
        Outcome::success(),
    );
    b.tool_start(&c, at(60), &Tool::apply_patch(Some("c0"), &["src/lib.rs"]));
    b.tool_finish(
        &c,
        at(61),
        &Tool::apply_patch(Some("c0"), &["src/lib.rs"]),
        Outcome::success(),
    );
    b.stop(&a, at(400));
    b.stop(&c, at(410));
    edit_turn(&mut b, &a, HOUR, "src/lib.rs", "n", "continue feature X");
    let p = project(&b.build());
    assert_eq!(p.work_units.len(), 2, "{:#?}", p.work_units);
    for u in &p.work_units {
        assert_eq!(u.sessions.len(), 1, "units stay per task");
    }
}

fn read_cargo(b: &mut Stream, s: &Sess, t: i64) {
    b.session_started(s, at(t));
    b.prompt(s, at(t + 5), "look at the manifest");
    b.tool_start(s, at(t + 6), &Tool::read(Some("r"), &["Cargo.toml"]));
    b.tool_finish(
        s,
        at(t + 7),
        &Tool::read(Some("r"), &["Cargo.toml"]),
        Outcome::success(),
    );
    b.stop(s, at(t + 20));
}

#[test]
fn reading_the_same_file_is_not_a_handoff() {
    let a = Sess::claude("a");
    let c = Sess::codex("c");
    let mut b = Stream::new();
    read_cargo(&mut b, &a, 0);
    b.session_ended(&a, at(30), "other");
    read_cargo(&mut b, &c, 20 * 60);
    let p = project(&b.build());
    assert!(
        p.handoffs.is_empty(),
        "two sessions that only read Cargo.toml twenty minutes apart: {:?}",
        p.handoffs
    );

    // Starting within five minutes, timing alone is weak evidence.
    let mut b = Stream::new();
    read_cargo(&mut b, &a, 0);
    b.session_ended(&a, at(30), "other");
    read_cargo(&mut b, &c, 120);
    let p = project(&b.build());
    assert_eq!(p.handoffs.len(), 1);
    assert_eq!(p.handoffs[0].confidence, 0.5);
    assert!(p.handoffs[0].shared_paths.is_empty());
}

#[test]
fn handoff_confidence_follows_the_rfc() {
    let a = Sess::claude("a");
    let c = Sess::codex("c");
    let paths = ["src/a.rs", "src/b.rs", "src/c.rs"];
    let build = |ended: bool, n: usize| {
        let mut b = Stream::new();
        b.session_started(&a, at(0));
        b.prompt(&a, at(1), "edit");
        for (i, p) in paths.iter().take(n).enumerate() {
            let id = format!("a{i}");
            b.tool_start(&a, at(2 + i as i64), &Tool::edit(Some(&id), &[*p]));
            b.tool_finish(
                &a,
                at(3 + i as i64),
                &Tool::edit(Some(&id), &[*p]),
                Outcome::success(),
            );
        }
        b.stop(&a, at(20));
        if ended {
            b.session_ended(&a, at(25), "exit");
        }
        b.session_started(&c, at(300));
        b.prompt(&c, at(301), "continue");
        for (i, p) in paths.iter().take(n).enumerate() {
            let id = format!("c{i}");
            b.tool_start(&c, at(302 + i as i64), &Tool::apply_patch(Some(&id), &[*p]));
            b.tool_finish(
                &c,
                at(303 + i as i64),
                &Tool::apply_patch(Some(&id), &[*p]),
                Outcome::success(),
            );
        }
        b.stop(&c, at(320));
        project(&b.build())
    };
    let conf = |p: &attemptdb_project::Projection| {
        assert_eq!(p.handoffs.len(), 1);
        p.handoffs[0].confidence
    };
    assert_eq!(conf(&build(false, 1)), 0.6, "base");
    assert_eq!(conf(&build(true, 1)), 0.8, "+0.2: the giver had ended");
    assert_eq!(conf(&build(false, 3)), 0.7, "+0.1: three shared paths");
    assert_eq!(conf(&build(true, 3)), 0.9, "capped at 0.9");
}

#[test]
fn a_round_trip_between_agents_is_two_handoffs() {
    let a = Sess::claude("a");
    let c = Sess::codex("c");
    let build = |resume: bool| {
        let mut b = Stream::new();
        b.session_started(&a, at(0));
        edit_turn(&mut b, &a, 1, "src/x.rs", "e", "implement");
        // A Codex review that only reads.
        b.session_started(&c, at(200));
        b.prompt(&c, at(201), "review x.rs");
        b.tool_start(&c, at(210), &Tool::read(Some("r"), &["src/x.rs"]));
        b.tool_finish(
            &c,
            at(211),
            &Tool::read(Some("r"), &["src/x.rs"]),
            Outcome::success(),
        );
        b.stop(&c, at(250));
        if resume {
            // Claude resumes the same session to apply the review.
            edit_turn(
                &mut b,
                &a,
                400,
                "src/x.rs",
                "e2",
                "apply the review comments",
            );
        }
        project(&b.build())
    };

    let p = build(false);
    assert_eq!(p.handoffs.len(), 1);
    assert_eq!(p.handoffs[0].from_session, a.session_id);
    assert_eq!(p.handoffs[0].to_session, c.session_id);

    let p = build(true);
    assert_eq!(
        p.handoffs.len(),
        2,
        "Claude to Codex to Claude: neither hand-over is lost: {:?}",
        p.handoffs
    );
    let (first, second) = (&p.handoffs[0], &p.handoffs[1]);
    assert_eq!(
        (first.from_session, first.to_session),
        (a.session_id, c.session_id)
    );
    assert_eq!(
        (second.from_session, second.to_session),
        (c.session_id, a.session_id)
    );
    assert!(second.at > first.at);
    let a_turns: Vec<_> = p.turns_of(a.session_id).collect();
    assert_eq!(
        second.to_turn,
        Some(a_turns[1].turn_id),
        "Claude's second turn received it"
    );
    // All three turns are one piece of work: the handoffs and the shared
    // edited file link them.
    assert_eq!(p.work_units.len(), 1);
    assert_eq!(p.work_units[0].turns.len(), 3);
    assert_eq!(
        p.edges
            .iter()
            .filter(|e| e.kind == attemptdb_project::EdgeKind::HandedOff)
            .count(),
        2
    );
}

#[test]
fn concurrent_agents_are_not_handoffs() {
    let a = Sess::claude("a");
    let c = Sess::codex("c");
    let mut b = Stream::new();
    b.session_started(&a, at(0));
    b.prompt(&a, at(1), "long task");
    b.tool_start(&a, at(2), &Tool::edit(Some("a0"), &["src/lib.rs"]));
    // Codex starts while Claude is still working.
    b.session_started(&c, at(50));
    b.prompt(&c, at(51), "other task");
    b.tool_start(&c, at(52), &Tool::apply_patch(Some("c0"), &["src/lib.rs"]));
    b.tool_finish(
        &c,
        at(53),
        &Tool::apply_patch(Some("c0"), &["src/lib.rs"]),
        Outcome::success(),
    );
    b.tool_finish(
        &a,
        at(100),
        &Tool::edit(Some("a0"), &["src/lib.rs"]),
        Outcome::success(),
    );
    b.stop(&a, at(110));
    let p = project(&b.build());
    assert!(p.handoffs.is_empty(), "{:?}", p.handoffs);
}

#[test]
fn a_conflict_is_keyed_by_the_repository_relative_path() {
    let a = Sess::claude("a");
    let c = Sess::codex("c");
    let mut b = Stream::new();
    b.session_started(&a, at(0));
    b.prompt(&a, at(1), "add feature X");
    b.session_started(&c, at(2));
    b.prompt(&c, at(3), "fix bug Y");
    for i in 0..3 {
        let t = 10 + i * 100;
        let (ca, cc) = (format!("a{i}"), format!("c{i}"));
        b.tool_start(&a, at(t), &Tool::edit(Some(&ca), &["src/lib.rs"]));
        b.tool_finish(
            &a,
            at(t + 1),
            &Tool::edit(Some(&ca), &["src/lib.rs"]),
            Outcome::success(),
        );
        b.tool_start(
            &c,
            at(t + 50),
            &Tool::apply_patch(Some(&cc), &["src/lib.rs"]),
        );
        b.tool_finish(
            &c,
            at(t + 51),
            &Tool::apply_patch(Some(&cc), &["src/lib.rs"]),
            Outcome::success(),
        );
    }
    let mut events: Vec<Event> = b.build();
    // The second device checked the repository out somewhere else.
    for e in events.iter_mut().filter(|e| e.session_id == c.session_id) {
        for p in e.paths.iter_mut() {
            p.logical = p.logical.replace("/work/repo", "/home/bob/repo");
        }
    }
    let p = project(&events);
    assert_eq!(p.work_units.len(), 2);
    assert_eq!(p.conflicts.len(), 1, "same file, different checkout roots");
    assert_eq!(p.conflicts[0].paths[0].path, "src/lib.rs");
}
