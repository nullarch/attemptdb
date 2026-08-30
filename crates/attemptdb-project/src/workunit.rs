//! Work units (`tier1-v0`, RFC 0003 §5.6).
//!
//! Within a project, turns are nodes of a graph. Two turns are linked when
//!
//! 1. they share at least one repository-relative path touched by a
//!    file-mutating or shell tool call,
//! 2. they are consecutive turns of the same session and the later one
//!    starts within [`LINK_WINDOW_US`] (ten minutes) of the earlier one's
//!    end, or
//! 3. a handoff links their sessions (the giving session's last turn is
//!    linked to the receiving session's first turn).
//!
//! Connected components are work units. Everything below is a heuristic:
//! confidence is the minimum over member attempts capped at
//! [`CONFIDENCE_CAP`], and no percentage of completion exists anywhere.
//!
//! **Phase** is judged from the unit's last [`PHASE_WINDOW`] tool calls
//! (chronological). Let the *decisive* calls be those whose category is
//! `shell`, `file_write`, `file_edit`, `notebook` or `plan`; reads, searches,
//! web, MCP, subagent and other calls are *neutral*.
//!
//! - An uncleared pending-input signal in a member session → `Blocked`.
//! - Otherwise the last decisive call in the window decides: failed (or
//!   denied) mutating/shell call → `Debug`; `git commit` / `git push` →
//!   `Deliver`; a `test` command after an earlier edit → `Verify` (a test
//!   run with no prior edit is `Explore`); a `plan` call → `Plan`; an edit
//!   followed by neutral calls → `Review`; an edit or other shell call as
//!   the very last call → `Implement`.
//! - No decisive call in the window: `Review` if the unit edited something
//!   before the window, else `Explore`.
//!
//! **Status** is independent of phase and judged against a reference time
//! (`now`): `Completed` when the last turn completed with a succeeding last
//! attempt, no tool call in flight, and the session ended or the unit has
//! been idle for over 30 minutes; `Abandoned` when the last attempt failed
//! or was abandoned and the unit has been idle for over two hours; `Unknown`
//! when every member session has unknown coverage; `Open` otherwise.

use crate::approach::path_key;
use crate::model::{
    Attempt, AttemptOutcome, CoverageGrade, Phase, Projection, Session, Signal, ToolCall, Turn,
    TurnStatus, WorkUnit, WorkUnitStatus,
};
use crate::projector::{is_given_up, is_mutating_or_shell};
use attemptdb_core::event::Provider;
use attemptdb_core::{
    AttemptId, EventId, Outcome, OutcomeStatus, ProjectId, SessionId, SpanId, Timestamp,
    ToolCategory, TurnId, WorkUnitId,
};
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::hash_map::Entry;

/// Consecutive turns of one session closer than this are one unit.
pub const LINK_WINDOW_US: i64 = 10 * 60 * 1_000_000;
/// Idle time after which a succeeded, stopped unit counts as completed.
pub const COMPLETE_IDLE_US: i64 = 30 * 60 * 1_000_000;
/// Idle time after which a failed or abandoned unit counts as abandoned.
pub const ABANDON_IDLE_US: i64 = 2 * 60 * 60 * 1_000_000;
/// Tool calls the phase is judged from.
pub const PHASE_WINDOW: usize = 5;
/// Ceiling for work-unit and decision confidence.
pub const CONFIDENCE_CAP: f32 = 0.7;
/// Confidence of a unit that (after retractions) has no attempt left.
const NO_ATTEMPT_CONFIDENCE: f32 = 0.4;

struct CallView<'a> {
    call: &'a ToolCall,
    /// Start (else finish) time: the order the phase window uses.
    at: Timestamp,
    /// Outcome as of the snapshot time (`None` while in flight).
    outcome: Option<&'a Outcome>,
    in_flight: bool,
}

struct AttemptView<'a> {
    attempt: &'a Attempt,
    outcome: AttemptOutcome,
}

struct TurnView<'a> {
    turn: &'a Turn,
    session: &'a Session,
    ended_at: Option<Timestamp>,
    status: TurnStatus,
    calls: Vec<CallView<'a>>,
    attempts: Vec<AttemptView<'a>>,
    /// Paths touched by mutating or shell calls, first-touch order.
    paths: Vec<String>,
    last_activity: Timestamp,
    objective: Option<String>,
}

fn latest(acc: &mut Timestamp, t: Option<Timestamp>) {
    if let Some(t) = t
        && t > *acc
    {
        *acc = t;
    }
}

fn find(parent: &mut [usize], mut i: usize) -> usize {
    while parent[i] != i {
        parent[i] = parent[parent[i]];
        i = parent[i];
    }
    i
}

fn union(parent: &mut [usize], a: usize, b: usize) {
    let (ra, rb) = (find(parent, a), find(parent, b));
    if ra != rb {
        // Attach the later root under the earlier one so roots stay stable.
        let (lo, hi) = if ra < rb { (ra, rb) } else { (rb, ra) };
        parent[hi] = lo;
    }
}

/// Build the work units of a projection. With `at = Some(t)` only entities
/// observed at or before `t` take part and outcomes are masked to what was
/// known at `t`; `now` is the reference time for idleness.
pub(crate) fn build(p: &Projection, at: Option<Timestamp>, now: Timestamp) -> Vec<WorkUnit> {
    macro_rules! lap {
        ($name:expr) => {
            if _prof {
                eprintln!(
                    "profile   wu/{:<20} {:>8.1} ms",
                    $name,
                    _t.elapsed().as_secs_f64() * 1e3
                );
                _t = std::time::Instant::now();
            }
        };
    let visible = |t: Timestamp| at.is_none_or(|a| t <= a);
    let sessions: HashMap<SessionId, &Session> =
        p.sessions.iter().map(|s| (s.session_id, s)).collect();
    let calls: HashMap<SpanId, &ToolCall> =
        p.tool_calls.iter().map(|c| (c.tool_call_id, c)).collect();
    let mut attempts_by_turn: HashMap<TurnId, Vec<&Attempt>> = HashMap::new();
    for a in &p.attempts {
        attempts_by_turn.entry(a.turn_id).or_default().push(a);
    }

    let mut turns: Vec<TurnView<'_>> = Vec::new();
    for t in &p.turns {
        if !visible(t.started_at) {
            continue;
        }
        let Some(session) = sessions.get(&t.session_id).copied() else {
            continue;
        };
        let ended_at = t.ended_at.filter(|e| visible(*e));
        let status = if ended_at.is_some() {
            t.status
        } else {
            TurnStatus::InProgress
        };
        let mut last_activity = t.started_at;
        latest(&mut last_activity, ended_at);
        let mut cviews = Vec::new();
        let mut paths: Vec<String> = Vec::new();
        // Insertion-ordered set: `Vec::contains` made this quadratic in the
        // number of paths a busy turn touches.
        let mut seen_paths: HashSet<String> = HashSet::new();
        for id in &t.tool_call_ids {
            let Some(c) = calls.get(id).copied() else {
                continue;
            };
            let Some(observed) = c.started_at.or(c.finished_at) else {
                continue;
            };
            if !visible(observed) {
                continue;
            }
            let finished = c.finished_at.filter(|f| visible(*f));
            latest(&mut last_activity, c.started_at.filter(|s| visible(*s)));
            latest(&mut last_activity, finished);
            if is_mutating_or_shell(c.tool.category) {
                for pth in &c.paths {
                    let key = path_key(pth);
                    if seen_paths.insert(key.clone()) {
                        paths.push(key);
                    }
                }
            }
            cviews.push(CallView {
                call: c,
                at: observed,
                outcome: if finished.is_some() {
                    c.outcome.as_ref()
                } else {
                    None
                },
                in_flight: c.started_at.is_some() && finished.is_none(),
            });
        }
        let mut aviews = Vec::new();
        for a in attempts_by_turn.get(&t.turn_id).into_iter().flatten() {
            if !visible(a.started_at) {
                continue;
            }
            let (outcome, _) = p.attempt_outcome_at(a, at);
            latest(&mut last_activity, a.ended_at.filter(|e| visible(*e)));
            aviews.push(AttemptView {
                attempt: a,
                outcome,
            });
        }
        let objective = match (&t.corrected, at) {
            (Some(c), Some(a)) if c.at > a => t.inferred_objective.clone(),
            _ => t.objective.clone(),
        };
        turns.push(TurnView {
            turn: t,
            session,
            ended_at,
            status,
            calls: cviews,
            attempts: aviews,
            paths,
            last_activity,
            objective,
        });
    }

    let n = turns.len();
    let mut parent: Vec<usize> = (0..n).collect();
    // Rule 1: shared paths within a project.
    let mut by_path: HashMap<(ProjectId, &str), usize> = HashMap::new();
    for (i, tv) in turns.iter().enumerate() {
        for pth in &tv.paths {
            match by_path.entry((tv.session.project_id, pth.as_str())) {
                Entry::Occupied(e) => union(&mut parent, i, *e.get()),
                Entry::Vacant(v) => {
                    v.insert(i);
                }
            }
        }
    }
    // Rule 2: consecutive turns of one session within the window.
    for i in 1..n {
        let (a, b) = (&turns[i - 1], &turns[i]);
        if a.session.session_id == b.session.session_id
            && let Some(e) = a.ended_at
            && b.turn.started_at.as_micros() - e.as_micros() <= LINK_WINDOW_US
        {
            union(&mut parent, i - 1, i);
        }
    }
    // Rule 3: handoffs link the giver's last turn to the receiver's first.
    let mut first_turn: HashMap<SessionId, usize> = HashMap::new();
    let mut last_turn: HashMap<SessionId, usize> = HashMap::new();
    for (i, tv) in turns.iter().enumerate() {
        first_turn.entry(tv.session.session_id).or_insert(i);
        last_turn.insert(tv.session.session_id, i);
    }
    for h in &p.handoffs {
        if !visible(h.at) {
            continue;
        }
        if let (Some(&x), Some(&y)) = (
            last_turn.get(&h.from_session),
            first_turn.get(&h.to_session),
        ) {
            union(&mut parent, x, y);
        }
    }

    let mut members: Vec<Vec<usize>> = Vec::new();
    let mut root_index: HashMap<usize, usize> = HashMap::new();
    for i in 0..n {
        let r = find(&mut parent, i);
        let idx = *root_index.entry(r).or_insert_with(|| {
            members.push(Vec::new());
            members.len() - 1
        });
        members[idx].push(i);
    }

    let mut units: Vec<WorkUnit> = members
        .iter()
        .map(|m| build_unit(p, &turns, m, at, now))
        .collect();
    units.sort_by_key(|a| (a.started_at, a.work_unit_id));
    units
}

fn build_unit(
    p: &Projection,
    turns: &[TurnView<'_>],
    members: &[usize],
    at: Option<Timestamp>,
    now: Timestamp,
) -> WorkUnit {
    let visible = |t: Timestamp| at.is_none_or(|a| t <= a);
    let earliest = members
        .iter()
        .copied()
        .min_by_key(|&i| (turns[i].turn.started_at, i))
        .expect("a component has at least one turn");
    let project = turns[earliest].session;
    let started_at = turns[earliest].turn.started_at;
    let first_evidence = turns[earliest].turn.first_event_id;
    let work_unit_id =
        WorkUnitId::derive(&[&project.project_id.to_string(), &first_evidence.to_string()]);

    let mut sessions: Vec<SessionId> = Vec::new();
    let mut actors: Vec<Provider> = Vec::new();
    let mut turn_ids: Vec<TurnId> = Vec::new();
    let mut attempt_ids: Vec<AttemptId> = Vec::new();
    let mut paths: Vec<String> = Vec::new();
    let mut seen_paths: HashSet<&str> = HashSet::new();
    let mut updated_at = started_at;
    let mut failure_count = 0u32;
    let mut evidence: Vec<EventId> = Vec::new();
    // Insertion-ordered set. A unit's evidence is every event of every
    // attempt in it, so a `Vec::contains` here was quadratic in the size of
    // a busy unit and dominated a whole projection.
    let mut evidence_set: HashSet<EventId> = HashSet::new();
    let mut push_evidence = |ev: &mut Vec<EventId>, id: EventId| {
        if evidence_set.insert(id) {
            ev.push(id);
        }
    };
    let mut confidence: Option<f32> = None;
    let mut last_attempt: Option<(Timestamp, usize, &AttemptView<'_>)> = None;
    let mut all_calls: Vec<&CallView<'_>> = Vec::new();
    let mut all_unknown = true;
    let mut order = 0usize;
    for &i in members {
        let tv = &turns[i];
        if !sessions.contains(&tv.session.session_id) {
            sessions.push(tv.session.session_id);
        }
        if !actors.contains(&tv.session.provider) {
            actors.push(tv.session.provider.clone());
        }
        if tv.session.coverage != CoverageGrade::Unknown {
            all_unknown = false;
        }
        turn_ids.push(tv.turn.turn_id);
        for pth in &tv.paths {
            if seen_paths.insert(pth.as_str()) {
                paths.push(pth.clone());
            }
        }
        latest(&mut updated_at, Some(tv.last_activity));
        for av in &tv.attempts {
            attempt_ids.push(av.attempt.attempt_id);
            if av.outcome.is_failure() {
                failure_count += 1;
            }
            for e in &av.attempt.evidence {
                push_evidence(&mut evidence, *e);
            }
            confidence = Some(
                confidence
                    .map(|c| c.min(av.attempt.confidence))
                    .unwrap_or(av.attempt.confidence),
            );
            order += 1;
            if last_attempt
                .as_ref()
                .is_none_or(|(t, o, _)| (av.attempt.started_at, order) > (*t, *o))
            {
                last_attempt = Some((av.attempt.started_at, order, av));
            }
        }
        all_calls.extend(tv.calls.iter());
    }
    all_calls.sort_by_key(|a| (a.at, a.call.tool_call_id));

    // Objective: the earliest prompted turn.
    let mut prompted: Vec<usize> = members
        .iter()
        .copied()
        .filter(|&i| turns[i].turn.prompt_event_id.is_some())
        .collect();
    prompted.sort_by_key(|&i| (turns[i].turn.started_at, i));
    let (objective, objective_event_id) = prompted
        .first()
        .map(|&i| (turns[i].objective.clone(), turns[i].turn.prompt_event_id))
        .unwrap_or((None, None));

    // Phase.
    let window_start = all_calls.len().saturating_sub(PHASE_WINDOW);
    let edited_before_window = all_calls[..window_start]
        .iter()
        .any(|c| c.call.tool.category.mutates_files());
    let pending: Option<&Signal> = p
        .signals
        .iter()
        .filter(|g| {
            sessions.contains(&g.session_id)
                && visible(g.at)
                && g.cleared_at.is_none_or(|c| !visible(c))
        })
        .max_by_key(|g| (g.at, g.event_id));
    let (phase, phase_reason) =
        decide_phase(&all_calls[window_start..], edited_before_window, pending);
    if let Some(g) = pending {
        push_evidence(&mut evidence, g.event_id);
    }
    for h in &p.handoffs {
        if visible(h.at) && sessions.contains(&h.from_session) && sessions.contains(&h.to_session) {
            for e in &h.evidence {
                push_evidence(&mut evidence, *e);
            }
        }
    }

    // Status.
    let last_turn = members
        .iter()
        .copied()
        .max_by_key(|&i| (turns[i].turn.started_at, i))
        .map(|i| &turns[i])
        .expect("a component has at least one turn");
    let in_flight = all_calls.iter().any(|c| c.in_flight);
    let session_ended = last_turn.session.ended_at.is_some_and(visible);
    let idle_us = (now.as_micros() - updated_at.as_micros()).max(0);
    let last_outcome = last_attempt.as_ref().map(|(_, _, av)| av.outcome);
    let idle_text = format!("{}s idle", idle_us / 1_000_000);
    let (status, status_reason) = if all_unknown {
        (
            WorkUnitStatus::Unknown,
            "every member session has unknown coverage".to_string(),
        )
    } else if last_turn.status == TurnStatus::Completed
        && last_outcome == Some(AttemptOutcome::Succeeded)
        && !in_flight
        && (session_ended || idle_us > COMPLETE_IDLE_US)
    {
        (
            WorkUnitStatus::Completed,
            format!(
                "last turn completed with a succeeded attempt, nothing in flight, {}",
                if session_ended {
                    "session ended".to_string()
                } else {
                    format!("{idle_text} (> 30 min)")
                }
            ),
        )
    } else if last_outcome.is_some_and(is_given_up) && idle_us > ABANDON_IDLE_US {
        (
            WorkUnitStatus::Abandoned,
            format!(
                "last attempt {} and {idle_text} (> 2 h)",
                last_outcome.map(|o| o.as_str()).unwrap_or("unknown")
            ),
        )
    } else {
        let mut why: Vec<String> = Vec::new();
        if in_flight {
            why.push("a tool call is in flight".into());
        }
        if last_turn.status == TurnStatus::InProgress {
            why.push("the last turn is in progress".into());
        }
        match last_outcome {
            Some(AttemptOutcome::Succeeded) if !session_ended => {
                why.push(format!("session open and {idle_text} (<= 30 min)"));
            }
            Some(o) if is_given_up(o) => {
                why.push(format!(
                    "last attempt {} and {idle_text} (<= 2 h)",
                    o.as_str()
                ));
            }
            Some(o) => why.push(format!("last attempt {}", o.as_str())),
            None => why.push("no attempt observed".into()),
        }
        (WorkUnitStatus::Open, why.join("; "))
    };
    let ended_at = match status {
        WorkUnitStatus::Completed | WorkUnitStatus::Abandoned => Some(updated_at),
        _ => None,
    };

    WorkUnit {
        work_unit_id,
        project_id: project.project_id,
        project_name: project.project_name.clone(),
        objective,
        objective_event_id,
        phase,
        phase_reason,
        status,
        status_reason,
        started_at,
        updated_at,
        ended_at,
        sessions,
        turns: turn_ids,
        attempts: attempt_ids,
        paths,
        actors,
        failure_count,
        last_attempt: last_attempt.map(|(_, _, av)| av.attempt.attempt_id),
        blocking_signal: pending.map(|g| g.event_id),
        evidence,
        confidence: confidence
            .map(|c| c.min(CONFIDENCE_CAP))
            .unwrap_or(NO_ATTEMPT_CONFIDENCE),
        algorithm_version: Default::default(),
        version: 1,
    }
}

fn is_decisive(category: ToolCategory) -> bool {
    is_mutating_or_shell(category) || category == ToolCategory::Plan
}

fn failed(outcome: Option<&Outcome>) -> bool {
    outcome.is_some_and(|o| matches!(o.status, OutcomeStatus::Failure | OutcomeStatus::Denied))
}

fn decide_phase(
    window: &[&CallView<'_>],
    edited_before_window: bool,
    pending: Option<&Signal>,
) -> (Phase, String) {
    if let Some(g) = pending {
        return (
            Phase::Blocked,
            format!(
                "pending-input signal `{}` raised at {} has not been cleared",
                g.signal_type.as_deref().unwrap_or(g.kind.as_str()),
                g.at
            ),
        );
    }
    let decisive = window
        .iter()
        .rposition(|c| is_decisive(c.call.tool.category));
    let Some(i) = decisive else {
        return if edited_before_window {
            (
                Phase::Review,
                format!(
                    "only read-only or neutral calls in the last {} after earlier edits",
                    window.len()
                ),
            )
        } else {
            (
                Phase::Explore,
                "no file-mutating, shell or plan call yet".to_string(),
            )
        };
    };
    let c = window[i];
    let category = c.call.tool.category;
    let name = &c.call.tool.name;
    let tail = window.len() - i - 1;
    let edited_before = edited_before_window
        || window[..i]
            .iter()
            .any(|x| x.call.tool.category.mutates_files());
    if is_mutating_or_shell(category) && failed(c.outcome) {
        let class = c.outcome.and_then(|o| o.class.clone()).unwrap_or_else(|| {
            c.outcome
                .map(|o| o.status.as_str().to_string())
                .unwrap_or_default()
        });
        return (
            Phase::Debug,
            format!(
                "the last decisive call ({name}, {}) failed with `{class}`",
                category.as_str()
            ),
        );
    }
    if category == ToolCategory::Shell {
        if let Some(sub) = c.call.git_subcommand.as_deref()
            && matches!(sub, "commit" | "push")
        {
            return (
                Phase::Deliver,
                format!("the last decisive call was `git {sub}`"),
            );
        }
        if c.call.command_category.as_deref() == Some("test") {
            return if edited_before {
                (
                    Phase::Verify,
                    "the last decisive call was a test command after edits".to_string(),
                )
            } else {
                (
                    Phase::Explore,
                    "a test command ran with no edit before it".to_string(),
                )
            };
        }
        return (
            Phase::Implement,
            format!("the last decisive call was a shell command ({name})"),
        );
    }
    if category == ToolCategory::Plan {
        return (
            Phase::Plan,
            format!("the last decisive call used a plan tool ({name})"),
        );
    }
    if tail > 0 {
        (
            Phase::Review,
            format!("{tail} read-only or neutral call(s) after the last edit ({name})"),
        )
    } else {
        (
            Phase::Implement,
            format!("the last call was an edit ({name}, {})", category.as_str()),
        )
    }
}
