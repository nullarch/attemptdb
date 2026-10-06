//! Cross-provider handoff detection (RFC 0003 §5.5).
//!
//! The unit of detection is a *turn span*: the stretch of time from a turn's
//! first event to its last. A handoff is a **receiving span** whose most
//! recent predecessor in the project belongs to a session of a *different
//! provider* that had gone quiet before it began:
//!
//! 1. both spans are in the same project;
//! 2. the project was quiet when the receiving span began: no other span
//!    covers that instant (two agents working at once are concurrent, not a
//!    handoff);
//! 3. the span that ended last before it (the *giving* span) belongs to a
//!    different session and a different provider (same-provider successions
//!    are continuations, which are not handoffs);
//! 4. the receiving span begins within 30 minutes of the giving span's end
//!    (or of the giving session's `SessionEnded`, whichever is later but
//!    still before it), and the receiving session touched a path the giving
//!    session had edited by then. Confidence follows RFC 0003 §5.5: `0.6`,
//!    `+0.2` when the giving session had ended, `+0.1` for three or more
//!    shared paths, capped at `0.9`. A path both sessions only read is not
//!    shared evidence: reading `Cargo.toml` after somebody else read it
//!    hands nothing over;
//! 5. or, with no shared path, the receiving span begins within 5 minutes
//!    (confidence `0.5`).
//!
//! Because the receiver is a span and not a whole session, a session that
//! resumes after another agent worked is a receiving session again:
//! Claude → Codex review → Claude resumes yields two handoffs, where the
//! old whole-session rule (an agent that is active again after the other
//! started can never have handed anything over) yielded none. For a
//! session's *first* span the receiving instant is the session's start.
//!
//! Finding the giving span is a binary search over each project's spans
//! sorted by end time, with a prefix maximum over spans sorted by start for
//! the quietness check, so detection is `O(spans log spans)` — not the
//! quadratic every-session-against-every-session scan it replaced.

use crate::model::Handoff;
use attemptdb_core::event::Provider;
use attemptdb_core::{EventId, ProjectId, SessionId, Timestamp, TurnId};
use std::collections::{BTreeMap, HashMap};

pub(crate) const SHARED_PATH_WINDOW_US: i64 = 30 * 60 * 1_000_000;
pub(crate) const QUICK_WINDOW_US: i64 = 5 * 60 * 1_000_000;

// Confidence is built in tenths so that it stays on the palette of RFC 0003
// (`0.6`, `0.7`, `0.8`, `0.9`) instead of drifting through float addition.
/// Confidence, in tenths, of a handoff with a shared edited path.
const BASE_TENTHS: u8 = 6;
/// Bonus when the giving session had ended before the receiver began.
const ENDED_BONUS_TENTHS: u8 = 2;
/// Bonus when at least [`MANY_SHARED_PATHS`] paths are shared.
const MANY_PATHS_BONUS_TENTHS: u8 = 1;
const MANY_SHARED_PATHS: usize = 3;
const CEILING_TENTHS: u8 = 9;
/// Confidence when only timing links the sessions.
const QUICK_CONFIDENCE: f32 = 0.5;

/// What one session did to one path.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PathTouch {
    /// First and last tool event that reported the path.
    pub first: EventId,
    pub last: EventId,
    /// When the last of those events happened.
    pub last_at: Timestamp,
    /// The earliest edit (file write, edit or notebook call) of the path,
    /// when the session edited it at all. A path that was only read is
    /// `None`.
    pub first_edit_at: Option<Timestamp>,
}

/// One turn's stretch of activity.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TurnSpan {
    pub turn_id: TurnId,
    pub start: Timestamp,
    pub end: Timestamp,
    pub first_event: EventId,
    pub last_event: EventId,
}

pub(crate) struct HandoffInput {
    pub session_id: SessionId,
    pub provider: Provider,
    pub project_id: ProjectId,
    pub started_at: Timestamp,
    pub ended_at: Option<Timestamp>,
    /// The session's first event, and its `SessionEnded` when observed:
    /// the evidence for the receiving and the giving side of a handoff.
    pub first_event_id: EventId,
    pub end_event_id: Option<EventId>,
    /// The session's turns, in order.
    pub spans: Vec<TurnSpan>,
    pub paths: BTreeMap<String, PathTouch>,
    /// A session with neither prompts nor tool calls (capture tests, stray
    /// lifecycle events) can neither give nor receive work.
    pub active: bool,
}

struct SpanRef<'a> {
    input: &'a HandoffInput,
    span: &'a TurnSpan,
    /// When the span begins as a receiver: the session's start for its
    /// first span, the span's start otherwise.
    begins: Timestamp,
    first: bool,
}

pub(crate) fn detect(inputs: &[HandoffInput]) -> Vec<Handoff> {
    let mut by_project: HashMap<ProjectId, Vec<SpanRef<'_>>> = HashMap::new();
    for input in inputs.iter().filter(|i| i.active) {
        for (n, span) in input.spans.iter().enumerate() {
            let first = n == 0;
            let begins = if first {
                input.started_at.min(span.start)
            } else {
                span.start
            };
            by_project
                .entry(input.project_id)
                .or_default()
                .push(SpanRef {
                    input,
                    span,
                    begins,
                    first,
                });
        }
    }

    let mut out = Vec::new();
    for spans in by_project.values_mut() {
        detect_in_project(spans, &mut out);
    }
    out.sort_by(|x, y| {
        (x.at, x.to_session, x.from_session, x.to_turn).cmp(&(
            y.at,
            y.to_session,
            y.from_session,
            y.to_turn,
        ))
    });
    out
}

fn detect_in_project(spans: &mut [SpanRef<'_>], out: &mut Vec<Handoff>) {
    // Receivers in order of beginning; the prefix maximum of ends answers
    // "was anybody active when this began".
    spans.sort_by_key(|s| (s.begins, s.input.session_id, s.span.start, s.span.turn_id));
    let mut prefix_max_end: Vec<Timestamp> = Vec::with_capacity(spans.len());
    let mut running = Timestamp::from_micros(i64::MIN);
    for s in spans.iter() {
        running = running.max(s.span.end);
        prefix_max_end.push(running);
    }
    // Candidate givers in order of end.
    let mut by_end: Vec<usize> = (0..spans.len()).collect();
    by_end.sort_by_key(|&i| {
        (
            spans[i].span.end,
            spans[i].input.session_id,
            spans[i].span.turn_id,
        )
    });

    for (r_pos, r) in spans.iter().enumerate() {
        // 2. Anybody active at the instant the receiver begins, other than
        // the receiver itself, makes this concurrency rather than handoff.
        // The prefix maximum includes the receiver's own span, which ends no
        // earlier than it begins; only an *earlier* span reaching past
        // `begins` counts, so look at the maximum over spans before it.
        if r_pos > 0 && prefix_max_end[r_pos - 1] > r.begins {
            continue;
        }
        // 3. The span that ended last before the receiver began.
        let upto = by_end.partition_point(|&i| spans[i].span.end <= r.begins);
        let giver = by_end[..upto].iter().rev().map(|&i| &spans[i]).find(|g| {
            !(g.input.session_id == r.input.session_id && g.span.turn_id == r.span.turn_id)
        });
        let Some(g) = giver else { continue };
        if g.input.session_id == r.input.session_id || g.input.provider == r.input.provider {
            continue;
        }
        if let Some(h) = judge(g, r) {
            out.push(h);
        }
    }
}

fn judge(g: &SpanRef<'_>, r: &SpanRef<'_>) -> Option<Handoff> {
    // The giver's last activity, or its end when that came later but still
    // before the receiver.
    let g_last = match g.input.ended_at {
        Some(e) if e <= r.begins => e.max(g.span.end),
        _ => g.span.end,
    };
    let gap = r.begins.as_micros() - g_last.as_micros();
    if gap < 0 {
        return None;
    }
    let giver_ended = g.input.ended_at.is_some_and(|e| e <= r.begins);

    // What the giver had edited by then that the receiver touched afterwards.
    let shared: Vec<&String> = g
        .input
        .paths
        .iter()
        .filter(|(path, touch)| {
            touch.first_edit_at.is_some_and(|t| t <= r.begins)
                && r.input
                    .paths
                    .get(path.as_str())
                    .is_some_and(|rt| rt.last_at >= r.begins)
        })
        .map(|(path, _)| path)
        .collect();

    let confidence = if !shared.is_empty() && gap <= SHARED_PATH_WINDOW_US {
        let mut tenths = BASE_TENTHS;
        if giver_ended {
            tenths += ENDED_BONUS_TENTHS;
        }
        if shared.len() >= MANY_SHARED_PATHS {
            tenths += MANY_PATHS_BONUS_TENTHS;
        }
        f32::from(tenths.min(CEILING_TENTHS)) / 10.0
    } else if gap <= QUICK_WINDOW_US {
        QUICK_CONFIDENCE
    } else {
        return None;
    };

    // The giving side: the end of its session when that is what made the
    // confidence higher, else its last event. The receiving side: the start
    // of the session for a first span, else the span's first event.
    let giver_event = match (giver_ended, g.input.end_event_id) {
        (true, Some(end)) => end,
        _ => g.span.last_event,
    };
    let receiver_event = if r.first {
        r.input.first_event_id
    } else {
        r.span.first_event
    };
    let mut evidence = vec![giver_event, receiver_event];
    if let Some(p) = shared.first() {
        evidence.push(g.input.paths[p.as_str()].last);
        evidence.push(r.input.paths[p.as_str()].first);
    }
    let mut dedup: Vec<EventId> = Vec::new();
    for e in evidence {
        if !dedup.contains(&e) {
            dedup.push(e);
        }
    }
    let shared_paths: Vec<String> = shared.iter().map(|s| (*s).clone()).collect();
    Some(Handoff {
        from_session: g.input.session_id,
        to_session: r.input.session_id,
        from_provider: g.input.provider.clone(),
        to_provider: r.input.provider.clone(),
        project_id: r.input.project_id,
        at: r.begins,
        gap_ms: (gap / 1_000) as u64,
        shared_paths,
        evidence: dedup,
        from_turn: Some(g.span.turn_id),
        to_turn: Some(r.span.turn_id),
        confidence,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn input(i: usize, provider: Provider, project: ProjectId) -> HandoffInput {
        let start = Timestamp::from_micros(i as i64 * 200 * 1_000_000);
        let end = Timestamp::from_micros(start.as_micros() + 100 * 1_000_000);
        let session_id = SessionId::derive(&["s", &i.to_string()]);
        HandoffInput {
            session_id,
            provider,
            project_id: project,
            started_at: start,
            ended_at: None,
            first_event_id: EventId::derive(&["first", &i.to_string()]),
            end_event_id: None,
            spans: vec![TurnSpan {
                turn_id: TurnId::derive(&["t", &i.to_string()]),
                start,
                end,
                first_event: EventId::derive(&["first", &i.to_string()]),
                last_event: EventId::derive(&["last", &i.to_string()]),
            }],
            paths: BTreeMap::new(),
            active: true,
        }
    }

    /// Sixty thousand sessions that alternate between two agents, each
    /// starting a minute and a half after the previous one went quiet: every
    /// one is a handoff. The quadratic scan that preceded the time-window
    /// search needed minutes for this; a sorted search needs well under a
    /// second even unoptimised.
    #[test]
    fn detection_does_not_compare_every_session_with_every_other() {
        let project = ProjectId::derive(&["p"]);
        let n = 60_000;
        let inputs: Vec<HandoffInput> = (0..n)
            .map(|i| {
                let provider = if i % 2 == 0 {
                    Provider::ClaudeCode
                } else {
                    Provider::Codex
                };
                input(i, provider, project)
            })
            .collect();
        let t = Instant::now();
        let out = detect(&inputs);
        let took = t.elapsed();
        assert_eq!(out.len(), n - 1);
        assert!(
            took.as_secs() < 20,
            "detecting {n} handoffs took {took:?}: is it quadratic again?"
        );
    }

    /// Sessions of one agent never hand over to each other.
    #[test]
    fn same_provider_successions_are_not_handoffs() {
        let project = ProjectId::derive(&["p"]);
        let inputs: Vec<HandoffInput> = (0..50)
            .map(|i| input(i, Provider::ClaudeCode, project))
            .collect();
        assert!(detect(&inputs).is_empty());
    }
}
