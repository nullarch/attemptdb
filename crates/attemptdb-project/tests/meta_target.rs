//! A reader that must decide whether a retraction or correction may be stored
//! (the sync server's ownership guard) has to read its target exactly as the
//! projector does. `meta_target` is that reading; this file holds it to the
//! projection: for every spelling, the projection retracts the session if and
//! only if `meta_target` names it.

use attemptdb_core::event::Provider;
use attemptdb_core::{CaptureMode, DeviceId, Event, EventKind, ProjectRef, SessionId};
use attemptdb_project::{MetaTargetRef, meta_target, project, retracted_ids};
use serde_json::json;

fn fact(dev: DeviceId, tag: &str) -> Event {
    Event::new(
        dev,
        Provider::ClaudeCode,
        "PostToolUse",
        EventKind::ToolCallFinished,
        ProjectRef::derive("/home/dev/p", None, &dev),
        format!("session-{tag}"),
        CaptureMode::MetadataOnly,
        "t",
    )
}

fn retraction(dev: DeviceId, attrs: &[(&str, &str)]) -> Event {
    let mut e = Event::new(
        dev,
        Provider::Other("attemptdb".into()),
        "Retraction",
        EventKind::Retraction,
        ProjectRef::derive("/home/dev/p", None, &dev),
        "meta",
        CaptureMode::MetadataOnly,
        "t",
    );
    for (k, v) in attrs {
        e.attrs.insert((*k).into(), json!(v));
    }
    e
}

#[test]
fn the_guard_reads_a_retraction_target_the_way_the_projection_acts_on_it() {
    let dev = DeviceId::derive(&["meta-target"]);
    let victim = fact(dev, "victim");
    let sid: SessionId = victim.session_id;
    let id = sid.to_string();
    let spellings = [
        ("session", format!("ses_{id}")),
        ("Session", id.clone()),
        ("SESSION", id.to_uppercase()),
        (" session ", format!("ses_{}", id.replace('-', ""))),
        ("session", format!("{{{id}}}")),
        ("session", format!("urn:uuid:{id}")),
        ("session", format!("ses_ses_{id}")),
        ("sessions", format!("ses_{id}")),
        ("", format!("ses_{id}")),
        ("session", "ses_nonsense".to_string()),
        ("session", String::new()),
        ("event", format!("ses_{id}")),
        ("turn", id.clone()),
        ("nonsense", id.clone()),
    ];
    let mut named = 0;
    for (ty, target) in &spellings {
        let r = retraction(dev, &[("target_type", ty), ("target", target)]);
        let read = meta_target(&r);
        let all = [victim.clone(), r.clone()];
        let acted = retracted_ids(&all).contains_session(&sid);
        assert_eq!(
            read == Some(MetaTargetRef::Session(sid)),
            acted,
            "target_type {ty:?}, target {target:?}: read {read:?}, projection retracted {acted}"
        );
        // And the projection really is the one that hides it.
        if acted {
            named += 1;
            assert!(
                project(all.iter())
                    .sessions
                    .iter()
                    .all(|s| s.session_id != sid)
            );
        }
    }
    assert!(
        named >= 7,
        "the spellings that matter were exercised: {named}"
    );
    // No declared type, prefix only.
    let r = retraction(dev, &[("target", &format!("ses_{id}"))]);
    assert_eq!(meta_target(&r), Some(MetaTargetRef::Session(sid)));
    let r = retraction(dev, &[("target", &id)]);
    assert_eq!(meta_target(&r), None, "a bare id needs a declared type");
    // Not a retraction or a correction: no target, whatever the attrs say.
    let mut f = fact(dev, "x");
    f.attrs.insert("target".into(), json!(format!("ses_{id}")));
    assert_eq!(meta_target(&f), None);
}

#[test]
fn a_correction_type_is_read_with_the_projectors_folding() {
    let dev = DeviceId::derive(&["meta-target-2"]);
    let victim = fact(dev, "victim");
    let attempt = attemptdb_core::AttemptId::derive(&["x"]);
    for ty in [
        "attempt_outcome",
        "Attempt_Outcome",
        "attempt-outcome",
        " ATTEMPT-OUTCOME ",
        "attempt_note",
    ] {
        let mut c = retraction(
            dev,
            &[("correction_type", ty), ("target", &attempt.to_string())],
        );
        c.kind = EventKind::Correction;
        assert_eq!(
            meta_target(&c),
            Some(MetaTargetRef::Attempt(attempt)),
            "{ty:?}"
        );
    }
    let turn = attemptdb_core::TurnId::derive(&["x"]);
    let mut c = retraction(
        dev,
        &[
            ("correction_type", "Turn-Objective"),
            ("target", &turn.to_string()),
        ],
    );
    c.kind = EventKind::Correction;
    assert_eq!(meta_target(&c), Some(MetaTargetRef::Turn(turn)));
    // An unknown correction type with a bare id names nothing.
    let mut c = retraction(
        dev,
        &[
            ("correction_type", "nonsense"),
            ("target", &attempt.to_string()),
        ],
    );
    c.kind = EventKind::Correction;
    assert_eq!(meta_target(&c), None);
    let _ = victim;
}
