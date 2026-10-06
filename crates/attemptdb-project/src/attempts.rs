//! Attempt splitting and supersession.
//!
//! Rules (`tier1-v5`):
//!
//! - A turn's tool calls are split **by agent** first: a main agent and the
//!   subagents it runs in parallel interleave their calls in one turn, and
//!   one agent's failure must not end another's attempt. Each agent's calls
//!   are then walked in order of first observation.
//! - An attempt ends when one of its calls **fails in a way that says the
//!   attempt did not work**, with outcome `failure` or `denied`
//!   (or, for a test run, a runner that exits `0` but reports failing
//!   tests, `attrs.tests_failed > 0`). The attempt is `Failed` with the
//!   call's failure class. Which failures count:
//!   - a file edit, write or notebook call: always, it is a state-changing
//!     step;
//!   - a shell command: when the attempt has edited something already (the
//!     command is checking, or building on, those edits), when the command
//!     is classified as a `test` or `build` run (explicitly verifying), or
//!     when it was denied;
//!   - anything else, and a shell command that is plain exploration
//!     (`grep` finding nothing, `ls` of a missing path, `cat`) before any
//!     edit: not a failed attempt. The call is still a failed *call*.
//!
//!   The next call that *started after* the failing call ended opens a new
//!   attempt; calls that were already running when the failure landed stay
//!   with the failed attempt.
//! - An agent's last attempt takes its outcome from the turn: `Succeeded`
//!   after a normal stop (or `Unknown` if it holds no tool call), `Failed`
//!   after `TurnFailed`, `Abandoned` when the turn was cut without a stop,
//!   `InProgress` while the turn is open. `Succeeded` means the turn
//!   stopped, nothing more: `Attempt::verification` says whether the attempt
//!   ran a test or build and how it went.
//! - Every turn yields at least one attempt so that "last attempt outcome"
//!   is always defined.
//! - A `Failed` attempt is `Superseded` by the first later attempt **of the
//!   same agent**, in the same turn or the next turn of the session, that
//!   either **edited** one of the paths it edited (a path that was only read
//!   is not a retry), or, when it failed a test or build run, passed a run
//!   of the same kind.
//! - An attempt's id comes from its evidence, never its position: see
//!   [`anchor`].

use crate::approach;
use crate::model::{
    AlgorithmVersion, Attempt, AttemptOutcome, CausalEdge, CoverageGrade, EdgeEndpoint, EdgeKind,
    HEURISTIC_EDGE_CONFIDENCE, ToolCall, ToolPairing, Turn, TurnStatus, Verification,
};
use attemptdb_core::{
    AgentId, AttemptId, EventId, OutcomeStatus, SessionId, Timestamp, ToolCategory,
};

pub(crate) struct TurnInput<'a> {
    pub session_id: SessionId,
    pub coverage: CoverageGrade,
    pub turn: &'a Turn,
    /// Failure class carried by a `TurnFailed` event, when any.
    pub turn_failure_class: Option<&'a str>,
    /// The turn's tool calls in order of first observation.
    pub calls: Vec<(&'a ToolCall, ToolPairing)>,
}

/// Internal bookkeeping needed for supersession edges.
#[derive(Clone, Debug)]
pub(crate) struct AttemptMeta {
    /// End (else start) event of the call that failed the attempt.
    pub failing_event_id: Option<EventId>,
    /// Start (else end) event of the attempt's first tool call.
    pub first_action_event_id: Option<EventId>,
    /// Paths the attempt edited (file write, edit or notebook calls),
    /// whether or not the edit succeeded: what a retry has to touch again.
    pub edited: Vec<String>,
    pub agent_id: AgentId,
    /// The kind of verification run (`test`, `build`) that failed the
    /// attempt, when one did.
    pub failed_check: Option<String>,
    /// The kinds of verification runs the attempt passed.
    pub passed_checks: Vec<String>,
}

#[derive(Default)]
struct Group<'a> {
    calls: Vec<(&'a ToolCall, ToolPairing)>,
    /// Index into `calls` of the call that ended the attempt by failing.
    failing: Option<usize>,
    /// Whether any call so far is a file edit, write or notebook call.
    has_edit: bool,
}

fn is_edit(call: &ToolCall) -> bool {
    call.tool.category.mutates_files()
}

/// How a shell call verified the work: `Some(kind)` for a test or build
/// run, `None` for any other call. The kind is `command_category` (`test`,
/// `build`), or `test` when the runner's summary was read without one.
fn check_kind(call: &ToolCall) -> Option<&str> {
    if call.tool.category != ToolCategory::Shell {
        return None;
    }
    match call.command_category.as_deref() {
        Some(k @ ("test" | "build")) => Some(k),
        _ if call.tests_failed.is_some() => Some("test"),
        _ => None,
    }
}

fn status_failed(call: &ToolCall) -> bool {
    call.outcome
        .as_ref()
        .is_some_and(|o| matches!(o.status, OutcomeStatus::Failure | OutcomeStatus::Denied))
}

/// Whether a verification run failed: it exited badly, or its runner
/// reported failing tests despite exiting `0`.
fn check_failed(call: &ToolCall) -> bool {
    status_failed(call) || call.tests_failed.is_some_and(|n| n > 0)
}

fn denied(call: &ToolCall) -> bool {
    call.outcome
        .as_ref()
        .is_some_and(|o| o.status == OutcomeStatus::Denied)
}

fn ends_attempt(call: &ToolCall, has_edit: bool) -> bool {
    let cat = call.tool.category;
    if cat.mutates_files() {
        return status_failed(call);
    }
    if cat != ToolCategory::Shell {
        return false;
    }
    if check_kind(call).is_some() {
        return check_failed(call);
    }
    status_failed(call) && (denied(call) || has_edit)
}

/// The failure class of the call that ended an attempt.
fn failure_class(call: &ToolCall) -> String {
    match &call.outcome {
        Some(o) if status_failed(call) => o
            .class
            .clone()
            .unwrap_or_else(|| o.status.as_str().to_string()),
        _ if call.tests_failed.is_some_and(|n| n > 0) => "tests_failed".to_string(),
        _ => "failure".to_string(),
    }
}

fn split_agent<'a>(
    turn: &Turn,
    calls: &[(&'a ToolCall, ToolPairing)],
) -> Vec<(Vec<(&'a ToolCall, ToolPairing)>, Option<usize>)> {
    let mut groups: Vec<Group<'a>> = Vec::new();
    let mut current = Group::default();
    // End time of the failure that closed `current`, if any.
    let mut boundary: Option<Timestamp> = None;
    for &(call, pairing) in calls {
        let observed_at = call.started_at.or(call.finished_at);
        if let Some(b) = boundary
            && observed_at.is_some_and(|t| t > b)
        {
            groups.push(std::mem::take(&mut current));
            boundary = None;
        }
        current.calls.push((call, pairing));
        current.has_edit |= is_edit(call);
        if current.failing.is_none() && ends_attempt(call, current.has_edit) {
            current.failing = Some(current.calls.len() - 1);
            boundary = Some(call.finished_at.or(observed_at).unwrap_or(turn.started_at));
        }
    }
    if !current.calls.is_empty() || groups.is_empty() {
        groups.push(current);
    }
    groups.into_iter().map(|g| (g.calls, g.failing)).collect()
}

pub(crate) fn split_turn(input: &TurnInput<'_>) -> Vec<(Attempt, AttemptMeta)> {
    // Calls by agent, in order of the agent's first call.
    let mut agents: Vec<(AgentId, Vec<(&ToolCall, ToolPairing)>)> = Vec::new();
    for &(call, pairing) in &input.calls {
        match agents.iter_mut().find(|(a, _)| *a == call.agent_id) {
            Some((_, calls)) => calls.push((call, pairing)),
            None => agents.push((call.agent_id, vec![(call, pairing)])),
        }
    }
    if agents.is_empty() {
        agents.push((AgentId::nil(), Vec::new()));
    }

    let mut built: Vec<(usize, usize, Attempt, AttemptMeta)> = Vec::new();
    for (agent_order, (agent_id, calls)) in agents.iter().enumerate() {
        let groups = split_agent(input.turn, calls);
        let n = groups.len();
        for (i, (calls, failing)) in groups.into_iter().enumerate() {
            let (attempt, meta) = build(
                input,
                *agent_id,
                agent_order == 0 && i == 0,
                i + 1 == n,
                calls,
                failing,
            );
            built.push((agent_order, i, attempt, meta));
        }
    }
    built.sort_by_key(|(order, i, a, _)| (a.started_at, *order, *i));
    built
        .into_iter()
        .enumerate()
        .map(|(index, (_, _, mut attempt, meta))| {
            attempt.index = index as u32;
            (attempt, meta)
        })
        .collect()
}

/// What an attempt's id is derived from: `(kind, event)`.
///
/// - `fail`: the end of the call that ended a failed attempt (its start when
///   no end was observed);
/// - `act`: the first tool-call event of any other attempt;
/// - `turn`: the turn's opening event, for an attempt with no tool call.
///
/// Every anchor is an event *inside* the attempt, so the id does not depend
/// on how many attempts precede it: an event arriving late and earlier than
/// the attempt (a transcript import, a second device) leaves it alone, a
/// scoped view of the log (`--since`) derives the same ids as the whole log,
/// and events appended to a running attempt do not change it.
fn anchor(
    turn: &Turn,
    calls: &[(&ToolCall, ToolPairing)],
    failing: Option<usize>,
) -> (&'static str, EventId) {
    if let Some(f) = failing {
        let c = calls[f].0;
        if let Some(e) = c.end_event_id.or(c.start_event_id) {
            return ("fail", e);
        }
    }
    if let Some(e) = calls
        .iter()
        .find_map(|(c, _)| c.start_event_id.or(c.end_event_id))
    {
        return ("act", e);
    }
    ("turn", turn.prompt_event_id.unwrap_or(turn.first_event_id))
}

fn build(
    input: &TurnInput<'_>,
    agent_id: AgentId,
    first_of_turn: bool,
    is_last: bool,
    calls: Vec<(&ToolCall, ToolPairing)>,
    failing: Option<usize>,
) -> (Attempt, AttemptMeta) {
    let turn = input.turn;

    let (outcome, failure_class, ended_at) = match failing {
        Some(f) => {
            let call = calls[f].0;
            (
                AttemptOutcome::Failed,
                Some(failure_class(call)),
                call.finished_at,
            )
        }
        None if is_last => match turn.status {
            TurnStatus::Completed => {
                let outcome = if calls.is_empty() {
                    AttemptOutcome::Unknown
                } else {
                    AttemptOutcome::Succeeded
                };
                (outcome, None, turn.ended_at)
            }
            TurnStatus::Failed => (
                AttemptOutcome::Failed,
                Some(
                    input
                        .turn_failure_class
                        .unwrap_or("turn_failed")
                        .to_string(),
                ),
                turn.ended_at,
            ),
            TurnStatus::Unknown => (AttemptOutcome::Abandoned, None, turn.ended_at),
            TurnStatus::InProgress => (AttemptOutcome::InProgress, None, None),
        },
        // Unreachable by construction: only a failure closes a non-final group.
        None => (AttemptOutcome::Unknown, None, None),
    };

    let started_at = if first_of_turn {
        turn.started_at
    } else {
        calls
            .first()
            .and_then(|(c, _)| c.started_at.or(c.finished_at))
            .unwrap_or(turn.started_at)
    };

    let mut evidence: Vec<EventId> = Vec::new();
    let mut push_evidence = |id: Option<EventId>| {
        if let Some(id) = id
            && !evidence.contains(&id)
        {
            evidence.push(id);
        }
    };
    push_evidence(turn.prompt_event_id);
    for (call, _) in &calls {
        push_evidence(call.start_event_id);
        push_evidence(call.end_event_id);
    }
    if is_last {
        push_evidence(turn.stop_event_id);
    }

    let mut paths: Vec<String> = Vec::new();
    let mut edited: Vec<String> = Vec::new();
    for (call, _) in &calls {
        for p in &call.paths {
            let key = approach::path_key(p);
            if is_edit(call) && !edited.contains(&key) {
                edited.push(key.clone());
            }
            if !paths.contains(&key) {
                paths.push(key);
            }
        }
    }

    let mut verification: Option<Verification> = None;
    let mut failed_check: Option<String> = None;
    let mut passed_checks: Vec<String> = Vec::new();
    for (call, _) in &calls {
        if let Some(kind) = check_kind(call) {
            if check_failed(call) {
                verification = Some(Verification::Failed);
                failed_check = Some(kind.to_string());
            } else if call.outcome.is_some() {
                verification = Some(Verification::Passed);
                passed_checks.push(kind.to_string());
            }
        }
    }

    let explicit_stop = matches!(turn.status, TurnStatus::Completed | TurnStatus::Failed);
    let clean_pairing = calls.iter().all(|(_, p)| *p == ToolPairing::CallId);
    let confidence = if matches!(
        input.coverage,
        CoverageGrade::Minimal | CoverageGrade::Unknown
    ) {
        0.4
    } else if !explicit_stop || !clean_pairing {
        0.6
    } else {
        0.9
    };

    let meta = AttemptMeta {
        failing_event_id: failing
            .and_then(|f| calls[f].0.end_event_id.or(calls[f].0.start_event_id)),
        first_action_event_id: calls
            .first()
            .and_then(|(c, _)| c.start_event_id.or(c.end_event_id)),
        edited,
        agent_id,
        failed_check,
        passed_checks,
    };

    let (kind, event) = anchor(turn, &calls, failing);
    let attempt = Attempt {
        commit_shas: Vec::new(),
        attempt_id: AttemptId::derive(&[&input.session_id.to_string(), kind, &event.to_string()]),
        session_id: input.session_id,
        turn_id: turn.turn_id,
        turn_index: turn.index,
        // Assigned once the turn's attempts are ordered.
        index: 0,
        agent_id,
        objective: turn.objective.clone(),
        approach: approach::summarise(calls.iter().map(|(c, _)| *c)),
        started_at,
        ended_at,
        outcome,
        failure_class,
        verification,
        tool_call_ids: calls.iter().map(|(c, _)| c.tool_call_id).collect(),
        paths,
        superseded_by: None,
        supersedes: None,
        evidence,
        confidence,
        algorithm_version: AlgorithmVersion::current(),
        work_unit_id: None,
        corrected: None,
        inferred_outcome: None,
        inferred_failure_class: None,
        note: None,
    };
    (attempt, meta)
}

/// Link failed attempts to the later attempt of the same agent that retried
/// them. `attempts` must be one session's attempts in `(turn index, index)`
/// order with `metas` parallel to it.
pub(crate) fn link_supersession(
    attempts: &mut [Attempt],
    metas: &[AttemptMeta],
    edges: &mut Vec<CausalEdge>,
) {
    for i in 0..attempts.len() {
        if attempts[i].outcome != AttemptOutcome::Failed {
            continue;
        }
        let from = &metas[i];
        if from.edited.is_empty() && from.failed_check.is_none() {
            continue;
        }
        let turn_index = attempts[i].turn_index;
        // Retried the same edit, or passed the check it had failed.
        let reason = |j: usize| -> Option<Retry> {
            let (candidate, to) = (&attempts[j], &metas[j]);
            if candidate.turn_index != turn_index && candidate.turn_index != turn_index + 1 {
                return None;
            }
            if to.agent_id != from.agent_id {
                return None;
            }
            if to.edited.iter().any(|p| from.edited.contains(p)) {
                return Some(Retry::SameEdit);
            }
            if from
                .failed_check
                .as_ref()
                .is_some_and(|k| to.passed_checks.contains(k))
            {
                return Some(Retry::PassedCheck);
            }
            None
        };
        let Some((j, retry)) = (i + 1..attempts.len()).find_map(|j| reason(j).map(|r| (j, r)))
        else {
            continue;
        };

        let from_id = attempts[i].attempt_id;
        let to_id = attempts[j].attempt_id;
        attempts[i].outcome = AttemptOutcome::Superseded;
        attempts[i].superseded_by = Some(to_id);
        if attempts[j].supersedes.is_none() {
            attempts[j].supersedes = Some(from_id);
        }

        let failing = metas[i].failing_event_id;
        let first = metas[j].first_action_event_id;
        let evidence: Vec<EventId> = failing.into_iter().chain(first).collect();
        let confidence = attempts[i].confidence;
        edges.push(CausalEdge {
            from: EdgeEndpoint::Attempt(from_id),
            to: EdgeEndpoint::Attempt(to_id),
            kind: EdgeKind::Superseded,
            evidence: evidence.clone(),
            confidence: match retry {
                Retry::SameEdit => confidence,
                Retry::PassedCheck => confidence.min(HEURISTIC_EDGE_CONFIDENCE),
            },
        });
        if let (Some(f), Some(n)) = (failing, first) {
            // "This failure led to that next action": adjacency plus a
            // shared edit or check, not an id the provider reported.
            edges.push(CausalEdge {
                from: EdgeEndpoint::Event(f),
                to: EdgeEndpoint::Event(n),
                kind: EdgeKind::Caused,
                evidence,
                confidence: confidence.min(HEURISTIC_EDGE_CONFIDENCE),
            });
        }
    }
}

#[derive(Clone, Copy)]
enum Retry {
    SameEdit,
    PassedCheck,
}
