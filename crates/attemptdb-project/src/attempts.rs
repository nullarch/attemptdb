//! Attempt splitting and supersession.
//!
//! Rules (v0):
//!
//! - A turn's tool calls are walked in order of first observation. An attempt
//!   ends when a call whose category mutates files (edit/write/notebook) or is
//!   a shell command ends with outcome `failure` or `denied`; that attempt is
//!   `Failed` with the call's failure class. The next call that *started
//!   after* the failing call ended opens a new attempt; calls that were
//!   already running when the failure landed stay with the failed attempt.
//! - The turn's last attempt takes its outcome from the turn: `Succeeded`
//!   after a normal stop (or `Unknown` if it holds no tool call), `Failed`
//!   after `TurnFailed`, `Abandoned` when the turn was cut without a stop,
//!   `InProgress` while the turn is open.
//! - Every turn yields at least one attempt so that "last attempt outcome"
//!   is always defined.
//! - A `Failed` attempt is `Superseded` by the first later attempt in the
//!   same turn or the next turn of the session that touches at least one of
//!   the same paths.

use crate::approach;
use crate::model::{
    AlgorithmVersion, Attempt, AttemptOutcome, CausalEdge, CoverageGrade, EdgeEndpoint, EdgeKind,
    ToolCall, Turn, TurnStatus,
};
use attemptdb_core::{AttemptId, EventId, OutcomeStatus, SessionId, Timestamp, ToolCategory};

/// How a tool call's start and end were matched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pairing {
    /// Start and end shared a provider call id.
    CallId,
    /// Matched by FIFO on `(agent, tool name)`.
    Fifo,
    /// End observed without a start.
    LoneFinish,
    /// Start observed without an end (in flight).
    Open,
}

pub(crate) struct TurnInput<'a> {
    pub session_id: SessionId,
    pub coverage: CoverageGrade,
    pub turn: &'a Turn,
    /// Failure class carried by a `TurnFailed` event, when any.
    pub turn_failure_class: Option<&'a str>,
    /// The turn's tool calls in order of first observation.
    pub calls: Vec<(&'a ToolCall, Pairing)>,
}

/// Internal bookkeeping needed for supersession edges.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AttemptMeta {
    /// End (else start) event of the call that failed the attempt.
    pub failing_event_id: Option<EventId>,
    /// Start (else end) event of the attempt's first tool call.
    pub first_action_event_id: Option<EventId>,
}

#[derive(Default)]
struct Group<'a> {
    calls: Vec<(&'a ToolCall, Pairing)>,
    /// Index into `calls` of the call that ended the attempt by failing.
    failing: Option<usize>,
}

fn ends_attempt(call: &ToolCall) -> bool {
    let cat = call.tool.category;
    if !(cat.mutates_files() || cat == ToolCategory::Shell) {
        return false;
    }
    call.outcome
        .as_ref()
        .is_some_and(|o| matches!(o.status, OutcomeStatus::Failure | OutcomeStatus::Denied))
}

pub(crate) fn split_turn(input: &TurnInput<'_>) -> Vec<(Attempt, AttemptMeta)> {
    let mut groups: Vec<Group<'_>> = Vec::new();
    let mut current = Group::default();
    // End time of the failure that closed `current`, if any.
    let mut boundary: Option<Timestamp> = None;
    for &(call, pairing) in &input.calls {
        let observed_at = call.started_at.or(call.finished_at);
        if let Some(b) = boundary
            && observed_at.is_some_and(|t| t > b)
        {
            groups.push(std::mem::take(&mut current));
            boundary = None;
        }
        current.calls.push((call, pairing));
        if current.failing.is_none() && ends_attempt(call) {
            current.failing = Some(current.calls.len() - 1);
            boundary = Some(
                call.finished_at
                    .or(observed_at)
                    .unwrap_or(input.turn.started_at),
            );
        }
    }
    if !current.calls.is_empty() || groups.is_empty() {
        groups.push(current);
    }
    let n = groups.len();
    groups
        .into_iter()
        .enumerate()
        .map(|(i, g)| build(input, i, n, g))
        .collect()
}

fn build(
    input: &TurnInput<'_>,
    index: usize,
    total: usize,
    group: Group<'_>,
) -> (Attempt, AttemptMeta) {
    let turn = input.turn;
    let is_last = index + 1 == total;

    let (outcome, failure_class, ended_at) = match group.failing {
        Some(f) => {
            let call = group.calls[f].0;
            let class = call
                .outcome
                .as_ref()
                .map(|o| {
                    o.class
                        .clone()
                        .unwrap_or_else(|| o.status.as_str().to_string())
                })
                .unwrap_or_else(|| "failure".to_string());
            (AttemptOutcome::Failed, Some(class), call.finished_at)
        }
        None if is_last => match turn.status {
            TurnStatus::Completed => {
                let outcome = if group.calls.is_empty() {
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

    let started_at = if index == 0 {
        turn.started_at
    } else {
        group
            .calls
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
    for (call, _) in &group.calls {
        push_evidence(call.start_event_id);
        push_evidence(call.end_event_id);
    }
    if is_last {
        push_evidence(turn.stop_event_id);
    }

    let mut paths: Vec<String> = Vec::new();
    for (call, _) in &group.calls {
        for p in &call.paths {
            let key = approach::path_key(p);
            if !paths.contains(&key) {
                paths.push(key);
            }
        }
    }

    let explicit_stop = matches!(turn.status, TurnStatus::Completed | TurnStatus::Failed);
    let clean_pairing = group.calls.iter().all(|(_, p)| *p == Pairing::CallId);
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
        failing_event_id: group.failing.and_then(|f| {
            group.calls[f]
                .0
                .end_event_id
                .or(group.calls[f].0.start_event_id)
        }),
        first_action_event_id: group
            .calls
            .first()
            .and_then(|(c, _)| c.start_event_id.or(c.end_event_id)),
    };

    let attempt = Attempt {
        commit_shas: Vec::new(),
        attempt_id: AttemptId::derive(&[
            &input.session_id.to_string(),
            &turn.index.to_string(),
            &index.to_string(),
        ]),
        session_id: input.session_id,
        turn_id: turn.turn_id,
        turn_index: turn.index,
        index: index as u32,
        objective: turn.objective.clone(),
        approach: approach::summarise(group.calls.iter().map(|(c, _)| *c)),
        started_at,
        ended_at,
        outcome,
        failure_class,
        tool_call_ids: group.calls.iter().map(|(c, _)| c.tool_call_id).collect(),
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

/// Link failed attempts to the later attempt that retried on the same paths.
/// `attempts` must be one session's attempts in `(turn index, index)` order
/// with `metas` parallel to it.
pub(crate) fn link_supersession(
    attempts: &mut [Attempt],
    metas: &[AttemptMeta],
    edges: &mut Vec<CausalEdge>,
) {
    for i in 0..attempts.len() {
        if attempts[i].outcome != AttemptOutcome::Failed || attempts[i].paths.is_empty() {
            continue;
        }
        let turn_index = attempts[i].turn_index;
        let later = (i + 1..attempts.len()).find(|&j| {
            let candidate = &attempts[j];
            (candidate.turn_index == turn_index || candidate.turn_index == turn_index + 1)
                && candidate
                    .paths
                    .iter()
                    .any(|p| attempts[i].paths.contains(p))
        });
        let Some(j) = later else { continue };

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
        edges.push(CausalEdge {
            from: EdgeEndpoint::Attempt(from_id),
            to: EdgeEndpoint::Attempt(to_id),
            kind: EdgeKind::Superseded,
            evidence: evidence.clone(),
        });
        if let (Some(f), Some(n)) = (failing, first) {
            edges.push(CausalEdge {
                from: EdgeEndpoint::Event(f),
                to: EdgeEndpoint::Event(n),
                kind: EdgeKind::Caused,
                evidence,
            });
        }
    }
}
