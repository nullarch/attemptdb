//! The projector: consumes events and produces a [`Projection`].
//!
//! [`Projector::push`] records a compact observation per event (no content
//! beyond the prompt text and correction notes); [`Projector::finish`] sorts
//! the observations defensively (see [`crate::order`]) and runs the whole
//! pipeline. Doing the reduction in `finish` keeps the output a pure function
//! of the event set, whatever order the caller pushed in.
//!
//! Pipeline (v0):
//!
//! 1. `Correction` / `Retraction` events are split off (they describe the
//!    log, they are not part of any session) and parsed ([`crate::meta`]).
//! 2. Fact events of retracted sessions and explicitly retracted events are
//!    set aside; the rest is grouped into sessions.
//! 3. Sessions, turns, tool calls, attempts and handoffs are projected.
//!    Retracted sessions are projected separately so the query layer can
//!    show them on request.
//! 4. Retracted attempts are removed from their sessions, corrections are
//!    applied in stream order, work units and decisions are derived
//!    ([`crate::workunit`], [`crate::decision`]).
//!
//! Per-session rules (v0):
//!
//! - `started_at` is the `SessionStarted` time, else the first event;
//!   `ended_at` is the `SessionEnded` time, else `None`.
//! - A `PromptSubmitted` opens a turn (index from `1`) and closes the previous
//!   one if it had no stop (status `Unknown`). `TurnStopped` / `TurnFailed`
//!   close the current turn with status `Completed` / `Failed`; a later stop
//!   before the next prompt moves the end forward. `SessionEnded` closes an
//!   open turn with status `Unknown`. Tool or stop events before any prompt
//!   create the implicit turn `0`. Events after a turn's end but before the
//!   next prompt are attributed to that turn.
//! - `ToolCallStarted` opens a tool call. `ToolCallFinished` / `ToolCallFailed`
//!   pair with the open call sharing its `tool.call_id`; otherwise FIFO over
//!   open calls with the same `(agent, tool name)` — restricted to calls
//!   *without* a call id when the end event has one, since two differing call
//!   ids denote two different calls. An end with no candidate becomes a
//!   complete call with `started_at = None`.
//! - Span ids: `SpanId::derive(&[session_id, "call", call_id])` when the
//!   call id is present and unused in the session, else
//!   `SpanId::derive(&[session_id, "seq", ordinal])`.

use crate::approach;
use crate::attempts::{self, AttemptMeta, Pairing, TurnInput};
use crate::conflict;
use crate::decision::{self, Denial};
use crate::handoff::{self, HandoffInput, PathTouch};
use crate::meta;
use crate::model::{
    AlgorithmVersion, Attempt, AttemptOutcome, CausalEdge, Commit, CoverageGrade, EdgeEndpoint,
    EdgeKind, Projection, ProjectionStats, RetractedEntities, RetractedSet, Session, Signal,
    ToolCall, Turn, TurnStatus, is_meta_kind,
};
use crate::model::{Correction, Retraction};
use crate::order::{self, OrderKey, OrderMode};
use crate::workunit;
use attemptdb_core::event::Provider;
use attemptdb_core::{
    AgentId, Event, EventId, EventKind, Outcome, OutcomeStatus, PortablePath, ProjectId, SessionId,
    SpanId, Timestamp, ToolCategory, ToolRef, TurnId,
};
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

/// Prefixes of client-injected "prompts" that are not human input. Kept in
/// sync with `attemptdb_adapters::common::INJECTED_PROMPT_PREFIXES`; the
/// projector cannot depend on the adapters crate.
const INJECTED_PROMPT_PREFIXES: &[&str] = &[
    "<task-notification>",
    "<system-reminder>",
    "<local-command-stdout>",
    "<local-command-caveat>",
    "<bash-stdout>",
    "<bash-stderr>",
    "[SYSTEM NOTIFICATION",
];

fn is_injected_prompt(o: &Obs) -> bool {
    o.prompt
        .as_deref()
        .map(str::trim_start)
        .is_some_and(|t| INJECTED_PROMPT_PREFIXES.iter().any(|p| t.starts_with(p)))
}

/// Content-free metadata keys the projector reads from [`Event::attrs`].
/// Adapters should populate one of the listed aliases; the first present key
/// wins.
pub mod attr_keys {
    /// `SessionStarted`: how the session began (`startup`, `resume`, ...).
    pub const START_SOURCE: &[&str] = &["source", "start_source"];
    /// `SessionEnded`: why the session ended.
    pub const END_REASON: &[&str] = &["reason", "end_reason"];
    /// `Notification`: the provider notification type.
    pub const NOTIFICATION_TYPE: &[&str] = &["notification_type", "type", "matcher"];
    /// `TurnFailed`: failure class when not carried by `outcome.class`.
    pub const FAILURE_CLASS: &[&str] = &["class", "failure_class", "reason"];
    /// `PromptSubmitted`: prompt length in characters (metadata-only mode).
    pub const PROMPT_CHARS: &[&str] = &["prompt_chars", "prompt_length", "prompt_len"];
    /// Notification types that leave the session waiting on a human.
    pub const BLOCKING_NOTIFICATION_TYPES: &[&str] =
        &["permission_prompt", "idle_prompt", "agent_needs_input"];
    /// Shell tool calls: the adapter's content-free command classification
    /// (`test`, `git`, `build`, ...) and git subcommand (`commit`, `push`).
    pub const COMMAND_CATEGORY: &[&str] = &["command_category"];
    pub const LINES_ADDED: &[&str] = &["lines_added"];
    pub const LINES_REMOVED: &[&str] = &["lines_removed"];
    pub const GIT_SUBCOMMAND: &[&str] = &["git_subcommand"];
    /// `Correction` events (RFC 0003 §8).
    pub const CORRECTION_TYPE: &[&str] = &["correction_type"];
    pub const CORRECTION_TARGET: &[&str] = &["target"];
    pub const CORRECTION_OUTCOME: &[&str] = &["outcome"];
    pub const CORRECTION_FAILURE_CLASS: &[&str] = &["failure_class"];
    /// `Retraction` events.
    pub const RETRACTION_TARGET_TYPE: &[&str] = &["target_type"];
    pub const RETRACTION_REASON: &[&str] = &["reason"];
    /// Both: length of the (content-gated) note.
    pub const NOTE_CHARS: &[&str] = &["note_chars"];
    /// Both: the content field carrying the human note.
    pub const NOTE_CONTENT: &[&str] = &["note", "message"];
}

fn first_attr_str(ev: &Event, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| ev.attrs.get(*k))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn first_attr_u64(ev: &Event, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|k| ev.attrs.get(*k))
        .and_then(Value::as_u64)
}

/// The metadata (and content-gated note) of a correction or retraction.
#[derive(Clone, Debug, Default)]
pub(crate) struct MetaObs {
    pub correction_type: Option<String>,
    pub target: Option<String>,
    pub target_type: Option<String>,
    pub outcome: Option<String>,
    pub failure_class: Option<String>,
    pub reason: Option<String>,
    pub note: Option<String>,
    pub note_chars: Option<u64>,
}

impl MetaObs {
    fn from_event(ev: &Event) -> Self {
        let note = ev.content.as_ref().and_then(|c| {
            attr_keys::NOTE_CONTENT
                .iter()
                .find_map(|k| c.extra.get(*k))
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| c.message.clone())
        });
        Self {
            correction_type: first_attr_str(ev, attr_keys::CORRECTION_TYPE),
            target: first_attr_str(ev, attr_keys::CORRECTION_TARGET),
            target_type: first_attr_str(ev, attr_keys::RETRACTION_TARGET_TYPE),
            outcome: first_attr_str(ev, attr_keys::CORRECTION_OUTCOME),
            failure_class: first_attr_str(ev, attr_keys::CORRECTION_FAILURE_CLASS),
            reason: first_attr_str(ev, attr_keys::RETRACTION_REASON),
            note_chars: first_attr_u64(ev, attr_keys::NOTE_CHARS)
                .or_else(|| note.as_ref().map(|n| n.chars().count() as u64)),
            note,
        }
    }
}

/// The subset of an event the projector needs.
#[derive(Clone, Debug)]
pub(crate) struct Obs {
    pub key: OrderKey,
    pub event_id: EventId,
    pub session_id: SessionId,
    pub provider: Provider,
    pub provider_session_id: String,
    pub project_id: ProjectId,
    pub project_name: String,
    pub kind: EventKind,
    pub at: Timestamp,
    pub agent_id: AgentId,
    pub tool: Option<ToolRef>,
    pub outcome: Option<Outcome>,
    pub duration_ms: Option<u64>,
    pub paths: Vec<PortablePath>,
    pub prompt: Option<String>,
    pub prompt_chars: Option<u64>,
    /// Kind-specific metadata: start source, end reason, notification type,
    /// or turn failure class.
    pub note: Option<String>,
    /// Shell command classification on tool events.
    pub command_category: Option<String>,
    pub git_subcommand: Option<String>,
    /// Edit size on file-edit tool events (`attrs.lines_added/removed`).
    pub lines_added: Option<u64>,
    pub lines_removed: Option<u64>,
    /// Repository `HEAD` / branch as the hook saw them when the event fired.
    pub head: Option<String>,
    pub branch: Option<String>,
    /// Present for `Correction` / `Retraction` events only.
    pub meta: Option<MetaObs>,
}

impl Obs {
    fn from_event(ev: &Event) -> Self {
        let note = match ev.kind {
            EventKind::SessionStarted => first_attr_str(ev, attr_keys::START_SOURCE),
            EventKind::SessionEnded => first_attr_str(ev, attr_keys::END_REASON),
            EventKind::Notification => first_attr_str(ev, attr_keys::NOTIFICATION_TYPE),
            EventKind::TurnFailed => ev
                .outcome
                .as_ref()
                .and_then(|o| o.class.clone())
                .or_else(|| first_attr_str(ev, attr_keys::FAILURE_CLASS)),
            _ => None,
        };
        let (prompt, prompt_chars) = if ev.kind == EventKind::PromptSubmitted {
            let prompt = ev.content.as_ref().and_then(|c| c.prompt.clone());
            let chars = first_attr_u64(ev, attr_keys::PROMPT_CHARS)
                .or_else(|| prompt.as_ref().map(|p| p.chars().count() as u64));
            (prompt, chars)
        } else {
            (None, None)
        };
        let is_tool = matches!(
            ev.kind,
            EventKind::ToolCallStarted
                | EventKind::ToolCallFinished
                | EventKind::ToolCallFailed
                | EventKind::PermissionDenied
        );
        Self {
            key: OrderKey::from_event(ev),
            event_id: ev.event_id,
            session_id: ev.session_id,
            provider: ev.provider.clone(),
            provider_session_id: ev.provider_session_id.clone(),
            project_id: ev.project.project_id,
            project_name: ev.project.name.clone(),
            kind: ev.kind,
            at: ev.observed_at,
            agent_id: ev.agent.agent_id,
            tool: if is_tool { ev.tool.clone() } else { None },
            outcome: if is_tool { ev.outcome.clone() } else { None },
            duration_ms: ev.duration_ms,
            paths: if is_tool {
                ev.paths.clone()
            } else {
                Vec::new()
            },
            prompt,
            prompt_chars,
            note,
            command_category: if is_tool {
                first_attr_str(ev, attr_keys::COMMAND_CATEGORY)
            } else {
                None
            },
            git_subcommand: if is_tool {
                first_attr_str(ev, attr_keys::GIT_SUBCOMMAND)
            } else {
                None
            },
            lines_added: if is_tool {
                first_attr_u64(ev, attr_keys::LINES_ADDED)
            } else {
                None
            },
            lines_removed: if is_tool {
                first_attr_u64(ev, attr_keys::LINES_REMOVED)
            } else {
                None
            },
            head: ev.project.head.clone(),
            branch: ev.project.branch.clone(),
            meta: if is_meta_kind(ev.kind) {
                Some(MetaObs::from_event(ev))
            } else {
                None
            },
        }
    }
}

/// Incremental projector: `push` events in any order, then `finish`.
#[derive(Debug, Default)]
pub struct Projector {
    obs: Vec<Obs>,
    events_seen: u64,
}

impl Projector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one event. Only the fields the projection needs are retained.
    pub fn push(&mut self, ev: &Event) {
        self.events_seen += 1;
        self.obs.push(Obs::from_event(ev));
    }

    /// Number of events pushed so far.
    pub fn len(&self) -> usize {
        self.obs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.obs.is_empty()
    }

    /// Sort the observations and build the projection. Work-unit status is
    /// judged against the latest observed timestamp in the stream, so the
    /// result is a pure function of the event set.
    pub fn finish(self) -> Projection {
        let now = self.obs.iter().map(|o| o.at).max().unwrap_or_default();
        self.finish_at(now)
    }

    /// Like [`Projector::finish`], judging idleness (work-unit status)
    /// against `now` instead of the stream's last timestamp.
    pub fn finish_at(self, now: Timestamp) -> Projection {
        let Projector {
            mut obs,
            events_seen,
        } = self;
        let mut stats = ProjectionStats {
            events_seen,
            ..Default::default()
        };

        let mode = order::choose_mode(obs.iter().map(|o| &o.key));
        stats.out_of_order_events = obs
            .windows(2)
            .filter(|w| w[1].key.compare(&w[0].key, mode) == Ordering::Less)
            .count() as u64;
        obs.sort_by(|a, b| a.key.compare(&b.key, mode));

        // 1. Corrections and retractions.
        let mut corrections = Vec::new();
        let mut retractions = Vec::new();
        for o in &obs {
            match o.kind {
                EventKind::Correction => corrections.push(meta::parse_correction(o)),
                EventKind::Retraction => retractions.push(meta::parse_retraction(o)),
                _ => {}
            }
        }
        stats.corrections_seen = corrections.len() as u64;
        stats.retractions_seen = retractions.len() as u64;
        let retracted_ids: RetractedSet = meta::retracted_set(&retractions);

        // 2. Partition the facts.
        let mut active: Vec<&Obs> = Vec::with_capacity(obs.len());
        let mut retracted_session_obs: Vec<&Obs> = Vec::new();
        for o in &obs {
            if o.meta.is_some() {
                continue;
            }
            if retracted_ids.contains_session(&o.session_id) {
                meta::note_session_match(&mut retractions, o.session_id);
                retracted_session_obs.push(o);
                stats.retracted_events += 1;
            } else if retracted_ids.contains_event(&o.event_id) {
                meta::note_event_match(&mut retractions, o.event_id);
                stats.retracted_events += 1;
            } else {
                active.push(o);
            }
        }

        // 3. Sessions, turns, tool calls, attempts.
        let builds = build_sessions(&active, &mut stats);
        let mut discard = ProjectionStats::default();
        let retracted_builds = build_sessions(&retracted_session_obs, &mut discard);
        drop(obs);

        assemble(
            builds,
            retracted_builds,
            corrections,
            retractions,
            retracted_ids,
            stats,
            now,
        )
    }
}

/// The cross-session half of a projection: handoffs, containment and
/// evidence edges, retractions and corrections, work units, decisions. Costs
/// O(sessions + turns + attempts), never O(events), which is what makes an
/// incremental refresh cheap once the per-session builds are cached.
fn assemble(
    builds: Vec<SessionBuild>,
    retracted_builds: Vec<SessionBuild>,
    corrections: Vec<Correction>,
    mut retractions: Vec<Retraction>,
    mut retracted_ids: RetractedSet,
    mut stats: ProjectionStats,
    now: Timestamp,
) -> Projection {
    let mut corrections = corrections;
    let handoff_inputs: Vec<HandoffInput> =
        builds.iter().map(SessionBuild::handoff_input).collect();
    let handoffs = handoff::detect(&handoff_inputs);

    let mut projection = Projection {
        commits: Vec::new(),
        algorithm_version: AlgorithmVersion::current(),
        sessions: Vec::with_capacity(builds.len()),
        turns: Vec::new(),
        tool_calls: Vec::new(),
        attempts: Vec::new(),
        handoffs: Vec::new(),
        edges: Vec::new(),
        signals: Vec::new(),
        work_units: Vec::new(),
        decisions: Vec::new(),
        corrections: Vec::new(),
        retractions: Vec::new(),
        conflicts: Vec::new(),
        retracted_ids: RetractedSet::default(),
        retracted: RetractedEntities::default(),
        reference_time: now,
        stats: ProjectionStats::default(),
        index: Default::default(),
    };

    let mut denials: Vec<Denial> = Vec::new();
    for b in builds {
        let session_id = b.session.session_id;
        for t in &b.turns {
            projection.edges.push(CausalEdge {
                from: EdgeEndpoint::Session(session_id),
                to: EdgeEndpoint::Turn(t.turn_id),
                kind: EdgeKind::ParentOf,
                evidence: vec![t.prompt_event_id.unwrap_or(t.first_event_id)],
            });
            if let Some(p) = t.prompt_event_id {
                projection.edges.push(CausalEdge {
                    from: EdgeEndpoint::Event(p),
                    to: EdgeEndpoint::Turn(t.turn_id),
                    kind: EdgeKind::Triggered,
                    evidence: vec![p],
                });
            }
        }
        for c in &b.calls {
            if let Some(turn_id) = c.turn_id {
                projection.edges.push(CausalEdge {
                    from: EdgeEndpoint::Turn(turn_id),
                    to: EdgeEndpoint::Span(c.tool_call_id),
                    kind: EdgeKind::ParentOf,
                    evidence: c.start_event_id.into_iter().chain(c.end_event_id).collect(),
                });
            }
        }
        for a in &b.attempts {
            for e in &a.evidence {
                projection.edges.push(CausalEdge {
                    from: EdgeEndpoint::Event(*e),
                    to: EdgeEndpoint::Attempt(a.attempt_id),
                    kind: EdgeKind::EvidenceFor,
                    evidence: vec![*e],
                });
            }
        }
        projection.edges.extend(b.supersession_edges);
        projection.sessions.push(b.session);
        projection.turns.extend(b.turns);
        projection.tool_calls.extend(b.calls);
        projection.attempts.extend(b.attempts);
        projection.commits.extend(b.commits);
        projection.signals.extend(b.signals);
        denials.extend(b.denials);
    }

    for h in &handoffs {
        projection.edges.push(CausalEdge {
            from: EdgeEndpoint::Session(h.from_session),
            to: EdgeEndpoint::Session(h.to_session),
            kind: EdgeKind::HandedOff,
            evidence: h.evidence.clone(),
        });
    }
    projection.handoffs = handoffs;

    for b in retracted_builds {
        projection.retracted.sessions.push(b.session);
        projection.retracted.turns.extend(b.turns);
        projection.retracted.tool_calls.extend(b.calls);
        projection.retracted.attempts.extend(b.attempts);
    }

    // 4. Retracted attempts, corrections, work units, decisions.
    meta::retract_attempts(
        &mut projection,
        &mut retractions,
        &mut retracted_ids,
        &mut stats,
    );
    meta::apply_corrections(
        &mut corrections,
        &mut projection.attempts,
        &mut projection.turns,
        &retracted_ids,
        &mut stats,
    );
    projection.corrections = corrections;
    projection.retractions = retractions;
    projection.retracted_ids = retracted_ids;

    projection.work_units = workunit::build(&projection, None, now);
    let unit_of: HashMap<attemptdb_core::AttemptId, attemptdb_core::WorkUnitId> = projection
        .work_units
        .iter()
        .flat_map(|u| u.attempts.iter().map(move |a| (*a, u.work_unit_id)))
        .collect();
    for a in &mut projection.attempts {
        a.work_unit_id = unit_of.get(&a.attempt_id).copied();
    }
    let shas_of: HashMap<attemptdb_core::AttemptId, &[String]> = projection
        .attempts
        .iter()
        .filter(|a| !a.commit_shas.is_empty())
        .map(|a| (a.attempt_id, a.commit_shas.as_slice()))
        .collect();
    for u in &mut projection.work_units {
        for aid in &u.attempts {
            for sha in shas_of.get(aid).copied().unwrap_or_default() {
                if !u.commit_shas.contains(sha) {
                    u.commit_shas.push(sha.clone());
                }
            }
        }
    }
    // Turn → the event that opened it, once; the work-unit loop below would
    // otherwise search every turn for every unit's turn.
    let turn_evidence: HashMap<TurnId, EventId> = projection
        .turns
        .iter()
        .map(|t| (t.turn_id, t.prompt_event_id.unwrap_or(t.first_event_id)))
        .collect();
    for u in &projection.work_units {
        for tid in &u.turns {
            let evidence = turn_evidence.get(tid).map(|e| vec![*e]).unwrap_or_default();
            projection.edges.push(CausalEdge {
                from: EdgeEndpoint::WorkUnit(u.work_unit_id),
                to: EdgeEndpoint::Turn(*tid),
                kind: EdgeKind::ParentOf,
                evidence,
            });
        }
    }
    projection.decisions = decision::derive(&projection, &denials, &unit_of);
    projection.conflicts = conflict::detect(&projection);
    projection.stats = stats;
    // The per-session index is derived from the rows above; none of the
    // steps here read through it, but if one ever does, what it built
    // would be stale by now.
    projection.index = Default::default();
    projection
}

/// Project a complete event stream. Input is sorted defensively.
pub fn project<'a>(events: impl IntoIterator<Item = &'a Event>) -> Projection {
    let mut p = Projector::new();
    for ev in events {
        p.push(ev);
    }
    p.finish()
}

/// Project only the events observed at or before `at`, judging work-unit
/// status against `at`: the projection as it would have been at that time.
pub fn project_at<'a>(events: impl IntoIterator<Item = &'a Event>, at: Timestamp) -> Projection {
    let mut p = Projector::new();
    for ev in events {
        if ev.observed_at <= at {
            p.push(ev);
        }
    }
    p.finish_at(at)
}

/// Group observations into sessions, finalize each, and sort by start.
fn build_sessions(obs: &[&Obs], stats: &mut ProjectionStats) -> Vec<SessionBuild> {
    let mut builds: Vec<SessionBuild> = Vec::new();
    let mut by_session: HashMap<SessionId, usize> = HashMap::new();
    for o in obs {
        let idx = *by_session.entry(o.session_id).or_insert_with(|| {
            builds.push(SessionBuild::new(o));
            builds.len() - 1
        });
        builds[idx].apply(o, stats);
    }
    for b in &mut builds {
        b.finalize(stats);
    }
    builds.sort_by(|a, b| {
        (a.session.started_at, a.session.session_id)
            .cmp(&(b.session.started_at, b.session.session_id))
    });
    builds
}

#[derive(Clone, Copy, Debug)]
struct CallMeta {
    /// Index into `SessionBuild::turns`.
    turn: usize,
    pairing: Pairing,
}

/// `HEAD` moved: the event that first showed the new value.
#[derive(Clone, Debug)]
struct HeadChange {
    seq: usize,
    event_id: EventId,
    from: Option<String>,
    to: String,
    consumed: bool,
}

/// A successful `git commit` call waiting for the sha it produced.
#[derive(Clone, Debug)]
struct PendingCommit {
    seq: usize,
    call_index: usize,
    end_event_id: EventId,
    at: Timestamp,
    head_before: Option<String>,
    /// The end event itself carried a new head.
    immediate: Option<String>,
    branch: Option<String>,
}

#[derive(Clone, Debug)]
struct SessionBuild {
    session: Session,
    turns: Vec<Turn>,
    /// Failure class from `TurnFailed`, parallel to `turns`.
    turn_failure_class: Vec<Option<String>>,
    calls: Vec<ToolCall>,
    call_meta: Vec<CallMeta>,
    open_by_call_id: HashMap<String, usize>,
    open_fifo: HashMap<(AgentId, String), VecDeque<usize>>,
    used_call_ids: HashSet<String>,
    current_turn: Option<usize>,
    next_turn_index: u32,
    signals: Vec<Signal>,
    open_signal: Option<usize>,
    path_touches: BTreeMap<String, PathTouch>,
    last_activity_at: Timestamp,
    attempts: Vec<Attempt>,
    supersession_edges: Vec<CausalEdge>,
    denials: Vec<Denial>,
    /// Position of the last observation within this session's stream.
    seq: usize,
    last_head: Option<String>,
    head_changes: Vec<HeadChange>,
    pending_commits: Vec<PendingCommit>,
    commits: Vec<Commit>,
}

impl SessionBuild {
    fn new(o: &Obs) -> Self {
        Self {
            session: Session {
                session_id: o.session_id,
                provider: o.provider.clone(),
                provider_session_id: o.provider_session_id.clone(),
                project_id: o.project_id,
                project_name: o.project_name.clone(),
                started_at: o.at,
                ended_at: None,
                end_reason: None,
                start_source: None,
                event_count: 0,
                turn_count: 0,
                prompt_count: 0,
                tool_call_count: 0,
                failure_count: 0,
                agents: Vec::new(),
                coverage: CoverageGrade::Unknown,
                first_event_id: o.event_id,
                last_event_id: o.event_id,
                last_event_at: o.at,
                start_event_id: None,
                end_event_id: None,
            },
            turns: Vec::new(),
            turn_failure_class: Vec::new(),
            calls: Vec::new(),
            call_meta: Vec::new(),
            open_by_call_id: HashMap::new(),
            open_fifo: HashMap::new(),
            used_call_ids: HashSet::new(),
            current_turn: None,
            next_turn_index: 1,
            signals: Vec::new(),
            open_signal: None,
            path_touches: BTreeMap::new(),
            last_activity_at: o.at,
            attempts: Vec::new(),
            supersession_edges: Vec::new(),
            denials: Vec::new(),
            seq: 0,
            last_head: None,
            head_changes: Vec::new(),
            pending_commits: Vec::new(),
            commits: Vec::new(),
        }
    }

    fn apply(&mut self, o: &Obs, stats: &mut ProjectionStats) {
        self.seq += 1;
        let s = &mut self.session;
        s.event_count += 1;
        s.last_event_id = o.event_id;
        s.last_event_at = o.at;
        if o.kind != EventKind::SessionEnded {
            self.last_activity_at = o.at;
        }
        if !o.agent_id.is_nil() && !s.agents.contains(&o.agent_id) {
            s.agents.push(o.agent_id);
        }
        // Any event ends a pending-input wait.
        if let Some(i) = self.open_signal.take() {
            self.signals[i].cleared_at = Some(o.at);
            self.signals[i].cleared_by = Some(o.event_id);
        }

        match o.kind {
            EventKind::SessionStarted => {
                if s.start_event_id.is_none() {
                    s.start_event_id = Some(o.event_id);
                    s.started_at = o.at;
                    s.start_source = o.note.clone();
                }
            }
            EventKind::SessionEnded => {
                s.end_event_id = Some(o.event_id);
                s.ended_at = Some(o.at);
                s.end_reason = o.note.clone();
                self.close_open_turn(o.at);
            }
            EventKind::PromptSubmitted if is_injected_prompt(o) => {
                // Client-injected messages (subagent task notifications,
                // local command output) reached the log as prompts from
                // adapters before 0.1.1. They never open a turn.
                stats.injected_prompts += 1;
            }
            EventKind::PromptSubmitted => {
                s.prompt_count += 1;
                self.close_open_turn(o.at);
                let index = self.next_turn_index;
                self.next_turn_index += 1;
                self.push_turn(o, index, Some(o.event_id));
            }
            EventKind::ToolCallStarted => self.tool_started(o),
            EventKind::ToolCallFinished | EventKind::ToolCallFailed => self.tool_ended(o, stats),
            EventKind::TurnStopped | EventKind::TurnFailed => {
                self.ensure_turn(o);
                let ti = self.current_turn.expect("ensure_turn creates a turn");
                let t = &mut self.turns[ti];
                t.stop_event_id = Some(o.event_id);
                t.ended_at = Some(o.at);
                if o.kind == EventKind::TurnFailed {
                    t.status = TurnStatus::Failed;
                    self.turn_failure_class[ti] = o.note.clone();
                    self.session.failure_count += 1;
                } else {
                    t.status = TurnStatus::Completed;
                }
            }
            EventKind::PermissionRequested => self.push_signal(o),
            EventKind::PermissionDenied => {
                self.ensure_turn(o);
                self.denials.push(Denial {
                    session_id: self.session.session_id,
                    event_id: o.event_id,
                    at: o.at,
                    tool_name: o.tool.as_ref().map(|t| t.name.clone()),
                    tool_call_id: None,
                });
            }
            EventKind::Notification => {
                if o.note
                    .as_deref()
                    .is_some_and(|t| attr_keys::BLOCKING_NOTIFICATION_TYPES.contains(&t))
                {
                    self.push_signal(o);
                }
            }
            EventKind::Unknown => stats.unknown_events += 1,
            _ => {}
        }

        if !matches!(o.kind, EventKind::SessionStarted | EventKind::SessionEnded)
            && let Some(ti) = self.current_turn
        {
            self.turns[ti].last_event_id = o.event_id;
        }

        // Repository HEAD, tracked after the event was applied so a commit
        // call sees the head *before* its own end event moved it.
        if let Some(h) = &o.head
            && self.last_head.as_deref() != Some(h.as_str())
        {
            self.head_changes.push(HeadChange {
                seq: self.seq,
                event_id: o.event_id,
                from: self.last_head.take(),
                to: h.clone(),
                consumed: false,
            });
            self.last_head = Some(h.clone());
        }
    }

    /// Tie each successful `git commit` call to the sha `HEAD` moved to.
    /// Runs after attempts exist so the commit can name its attempt.
    fn link_commits(&mut self) {
        let session_id = self.session.session_id;
        let project_id = self.session.project_id;
        let pending = std::mem::take(&mut self.pending_commits);
        for p in pending {
            let (tool_call_id, turn_id, start_event_id) = {
                let c = &self.calls[p.call_index];
                (c.tool_call_id, c.turn_id, c.start_event_id)
            };
            let mut evidence: Vec<EventId> = start_event_id.into_iter().collect();
            evidence.push(p.end_event_id);
            let (sha, linkage, confidence) = if let Some(sha) = p.immediate.clone() {
                (Some(sha), "end_event", 0.9)
            } else if let Some(hc) = self.head_changes.iter_mut().find(|hc| {
                !hc.consumed
                    && hc.seq > p.seq
                    && (p.head_before.is_none() || hc.from == p.head_before)
            }) {
                hc.consumed = true;
                evidence.push(hc.event_id);
                (Some(hc.to.clone()), "next_head", 0.7)
            } else {
                (None, "unresolved", 0.4)
            };
            let attempt_id = self
                .attempts
                .iter()
                .find(|a| a.tool_call_ids.contains(&tool_call_id))
                .map(|a| a.attempt_id);
            if let (Some(sha), Some(aid)) = (&sha, attempt_id)
                && let Some(a) = self.attempts.iter_mut().find(|a| a.attempt_id == aid)
                && !a.commit_shas.contains(sha)
            {
                a.commit_shas.push(sha.clone());
            }
            self.commits.push(Commit {
                commit_id: attemptdb_core::CommitId::derive(&[
                    "session",
                    &session_id.to_string(),
                    "call",
                    &tool_call_id.to_string(),
                ]),
                session_id,
                project_id,
                turn_id,
                attempt_id,
                tool_call_id,
                sha,
                previous_sha: p.head_before,
                branch: p.branch,
                at: p.at,
                linkage: linkage.to_string(),
                evidence,
                confidence,
                algorithm_version: AlgorithmVersion::current(),
            });
        }
    }

    fn push_turn(&mut self, o: &Obs, index: u32, prompt_event_id: Option<EventId>) {
        let session_id = self.session.session_id;
        self.turns.push(Turn {
            turn_id: TurnId::derive(&[&session_id.to_string(), &index.to_string()]),
            session_id,
            index,
            started_at: o.at,
            ended_at: None,
            prompt_event_id,
            stop_event_id: None,
            status: TurnStatus::InProgress,
            tool_call_ids: Vec::new(),
            objective: if prompt_event_id.is_some() {
                o.prompt.clone()
            } else {
                None
            },
            prompt_chars: if prompt_event_id.is_some() {
                o.prompt_chars
            } else {
                None
            },
            first_event_id: o.event_id,
            last_event_id: o.event_id,
            corrected: None,
            inferred_objective: None,
        });
        self.turn_failure_class.push(None);
        self.current_turn = Some(self.turns.len() - 1);
    }

    /// Create the implicit turn `0` when activity precedes any prompt.
    fn ensure_turn(&mut self, o: &Obs) {
        if self.current_turn.is_none() {
            self.push_turn(o, 0, None);
        }
    }

    /// Close the current turn without a stop event (cut by the next prompt or
    /// the session end).
    fn close_open_turn(&mut self, at: Timestamp) {
        if let Some(ti) = self.current_turn
            && self.turns[ti].ended_at.is_none()
        {
            self.turns[ti].ended_at = Some(at);
            self.turns[ti].status = TurnStatus::Unknown;
        }
    }

    fn push_signal(&mut self, o: &Obs) {
        self.signals.push(Signal {
            session_id: self.session.session_id,
            event_id: o.event_id,
            at: o.at,
            kind: o.kind,
            signal_type: if o.kind == EventKind::Notification {
                o.note.clone()
            } else {
                None
            },
            cleared_at: None,
            cleared_by: None,
        });
        self.open_signal = Some(self.signals.len() - 1);
    }

    fn tool_ref(o: &Obs) -> ToolRef {
        o.tool.clone().unwrap_or_else(|| ToolRef {
            name: "unknown".to_string(),
            category: ToolCategory::Other,
            call_id: None,
        })
    }

    fn span_id_for(&mut self, call_id: Option<&str>, ordinal: usize) -> SpanId {
        let sid = self.session.session_id.to_string();
        match call_id {
            Some(cid) if self.used_call_ids.insert(cid.to_string()) => {
                SpanId::derive(&[&sid, "call", cid])
            }
            _ => SpanId::derive(&[&sid, "seq", &ordinal.to_string()]),
        }
    }

    fn touch_paths(&mut self, o: &Obs) {
        for p in &o.paths {
            let key = approach::path_key(p);
            self.path_touches
                .entry(key)
                .and_modify(|t| t.last = o.event_id)
                .or_insert(PathTouch {
                    first: o.event_id,
                    last: o.event_id,
                });
        }
    }

    fn tool_started(&mut self, o: &Obs) {
        self.ensure_turn(o);
        let ti = self.current_turn.expect("ensure_turn creates a turn");
        let tool = Self::tool_ref(o);
        let ordinal = self.calls.len();
        let span = self.span_id_for(tool.call_id.as_deref(), ordinal);
        self.calls.push(ToolCall {
            tool_call_id: span,
            session_id: self.session.session_id,
            turn_id: Some(self.turns[ti].turn_id),
            agent_id: o.agent_id,
            tool: tool.clone(),
            started_at: Some(o.at),
            finished_at: None,
            duration_ms: None,
            outcome: None,
            paths: dedup_paths(&o.paths),
            start_event_id: Some(o.event_id),
            end_event_id: None,
            command_category: o.command_category.clone(),
            git_subcommand: o.git_subcommand.clone(),
            lines_added: o.lines_added,
            lines_removed: o.lines_removed,
        });
        self.call_meta.push(CallMeta {
            turn: ti,
            pairing: Pairing::Open,
        });
        self.turns[ti].tool_call_ids.push(span);
        if let Some(cid) = &tool.call_id {
            self.open_by_call_id.insert(cid.clone(), ordinal);
        }
        self.open_fifo
            .entry((o.agent_id, tool.name.clone()))
            .or_default()
            .push_back(ordinal);
        self.session.tool_call_count += 1;
        self.touch_paths(o);
    }

    fn tool_ended(&mut self, o: &Obs, stats: &mut ProjectionStats) {
        self.ensure_turn(o);
        let tool = Self::tool_ref(o);
        let outcome = o.outcome.clone().unwrap_or_else(|| {
            if o.kind == EventKind::ToolCallFailed {
                Outcome::failure(None)
            } else {
                Outcome::success()
            }
        });
        if matches!(
            outcome.status,
            OutcomeStatus::Failure | OutcomeStatus::Denied
        ) {
            self.session.failure_count += 1;
        }
        self.touch_paths(o);

        let matched = self.match_start(o.agent_id, &tool, stats);
        let call_index = match matched {
            Some((i, pairing)) => {
                let call = &mut self.calls[i];
                call.finished_at = Some(o.at);
                call.end_event_id = Some(o.event_id);
                // Edit sizes are known when the tool has run; the finish
                // carries them.
                if o.lines_added.is_some() || o.lines_removed.is_some() {
                    call.lines_added = o.lines_added.or(call.lines_added);
                    call.lines_removed = o.lines_removed.or(call.lines_removed);
                }
                call.duration_ms = o.duration_ms.or_else(|| {
                    call.started_at
                        .map(|st| (o.at.as_micros() - st.as_micros()).max(0) as u64 / 1_000)
                });
                call.outcome = Some(outcome);
                for p in &o.paths {
                    if !call.paths.contains(p) {
                        call.paths.push(p.clone());
                    }
                }
                if call.command_category.is_none() {
                    call.command_category = o.command_category.clone();
                }
                if call.git_subcommand.is_none() {
                    call.git_subcommand = o.git_subcommand.clone();
                }
                self.call_meta[i].pairing = pairing;
                i
            }
            None => {
                let ti = self.current_turn.expect("ensure_turn creates a turn");
                let ordinal = self.calls.len();
                let span = self.span_id_for(tool.call_id.as_deref(), ordinal);
                self.calls.push(ToolCall {
                    tool_call_id: span,
                    session_id: self.session.session_id,
                    turn_id: Some(self.turns[ti].turn_id),
                    agent_id: o.agent_id,
                    tool,
                    started_at: None,
                    finished_at: Some(o.at),
                    duration_ms: o.duration_ms,
                    outcome: Some(outcome),
                    paths: dedup_paths(&o.paths),
                    start_event_id: None,
                    end_event_id: Some(o.event_id),
                    command_category: o.command_category.clone(),
                    git_subcommand: o.git_subcommand.clone(),
                    lines_added: o.lines_added,
                    lines_removed: o.lines_removed,
                });
                self.call_meta.push(CallMeta {
                    turn: ti,
                    pairing: Pairing::LoneFinish,
                });
                self.turns[ti].tool_call_ids.push(span);
                self.session.tool_call_count += 1;
                stats.unpaired_tool_finishes += 1;
                ordinal
            }
        };
        let call = &self.calls[call_index];
        if call
            .outcome
            .as_ref()
            .is_some_and(|oc| oc.status == OutcomeStatus::Denied)
        {
            self.denials.push(Denial {
                session_id: self.session.session_id,
                event_id: o.event_id,
                at: o.at,
                tool_name: Some(call.tool.name.clone()),
                tool_call_id: Some(call.tool_call_id),
            });
        }
        if call.git_subcommand.as_deref() == Some("commit")
            && call
                .outcome
                .as_ref()
                .is_none_or(|oc| oc.status == OutcomeStatus::Success)
        {
            let immediate = o
                .head
                .clone()
                .filter(|h| self.last_head.as_deref() != Some(h.as_str()));
            self.pending_commits.push(PendingCommit {
                seq: self.seq,
                call_index,
                end_event_id: o.event_id,
                at: o.at,
                head_before: self.last_head.clone(),
                immediate,
                branch: o.branch.clone(),
            });
        }
    }

    /// Find the open call an end event belongs to and remove it from the
    /// open sets. Returns the call index and how it was matched.
    fn match_start(
        &mut self,
        agent_id: AgentId,
        tool: &ToolRef,
        stats: &mut ProjectionStats,
    ) -> Option<(usize, Pairing)> {
        if let Some(cid) = &tool.call_id
            && let Some(i) = self.open_by_call_id.remove(cid)
        {
            let key = (self.calls[i].agent_id, self.calls[i].tool.name.clone());
            if let Some(q) = self.open_fifo.get_mut(&key) {
                q.retain(|&x| x != i);
            }
            return Some((i, Pairing::CallId));
        }
        let key = (agent_id, tool.name.clone());
        let queue = self.open_fifo.get_mut(&key)?;
        let end_has_id = tool.call_id.is_some();
        let pos = queue
            .iter()
            .position(|&i| !end_has_id || self.calls[i].tool.call_id.is_none())?;
        let i = queue.remove(pos).expect("position is within the queue");
        if let Some(cid) = &self.calls[i].tool.call_id {
            self.open_by_call_id.remove(cid);
        }
        stats.fifo_pairings += 1;
        Some((i, Pairing::Fifo))
    }

    fn finalize(&mut self, stats: &mut ProjectionStats) {
        for t in &mut self.turns {
            if t.ended_at.is_none() {
                t.status = TurnStatus::InProgress;
            }
        }
        stats.unpaired_tool_starts += self
            .call_meta
            .iter()
            .filter(|m| m.pairing == Pairing::Open)
            .count() as u64;

        let s = &mut self.session;
        let has_start = s.start_event_id.is_some();
        let has_end = s.end_event_id.is_some();
        let has_prompt = s.prompt_count > 0;
        let has_tool = !self.calls.is_empty();
        s.coverage = match (
            has_start && has_end,
            has_start || has_end,
            has_prompt,
            has_tool,
        ) {
            (true, _, true, true) => CoverageGrade::Full,
            (_, true, p, t) if p || t => CoverageGrade::Partial,
            (_, false, p, t) if p || t => CoverageGrade::Minimal,
            _ => CoverageGrade::Unknown,
        };
        s.turn_count = self.turns.len() as u32;
        s.tool_call_count = self.calls.len() as u32;

        let session_id = s.session_id;
        let coverage = s.coverage;
        let mut metas: Vec<AttemptMeta> = Vec::new();
        for (ti, turn) in self.turns.iter().enumerate() {
            let calls = self
                .calls
                .iter()
                .zip(&self.call_meta)
                .filter(|(_, m)| m.turn == ti)
                .map(|(c, m)| (c, m.pairing))
                .collect();
            let input = TurnInput {
                session_id,
                coverage,
                turn,
                turn_failure_class: self.turn_failure_class[ti].as_deref(),
                calls,
            };
            for (attempt, meta) in attempts::split_turn(&input) {
                self.attempts.push(attempt);
                metas.push(meta);
            }
        }
        attempts::link_supersession(&mut self.attempts, &metas, &mut self.supersession_edges);
        self.link_commits();
    }

    fn handoff_input(&self) -> HandoffInput {
        HandoffInput {
            session_id: self.session.session_id,
            provider: self.session.provider.clone(),
            project_id: self.session.project_id,
            started_at: self.session.started_at,
            ended_at: self.session.ended_at,
            last_activity_at: self.last_activity_at,
            first_event_id: self.session.first_event_id,
            last_event_id: self.session.last_event_id,
            paths: self.path_touches.clone(),
            active: self.session.prompt_count > 0 || self.session.tool_call_count > 0,
        }
    }
}

fn dedup_paths(paths: &[PortablePath]) -> Vec<PortablePath> {
    let mut out: Vec<PortablePath> = Vec::with_capacity(paths.len());
    for p in paths {
        if !out.contains(p) {
            out.push(p.clone());
        }
    }
    out
}

/// Whether a tool call's category can change the working tree (or, for a
/// shell command, might).
pub(crate) fn is_mutating_or_shell(category: ToolCategory) -> bool {
    category.mutates_files() || category == ToolCategory::Shell
}

/// Whether an attempt outcome counts as a failure or abandonment for status
/// purposes.
pub(crate) fn is_given_up(outcome: AttemptOutcome) -> bool {
    outcome.is_failure() || outcome == AttemptOutcome::Abandoned
}

/// Incremental projection.
///
/// Sessions are independent of one another: a session's turns, tool calls,
/// attempts and signals depend only on its own events. Everything that
/// crosses sessions — handoffs, work units, decisions, the edge list — is
/// derived from finished session builds and costs O(sessions), not
/// O(events). So a refresh after new events only has to rebuild the sessions
/// those events touched, then re-run the cheap cross-session stage.
///
/// Three things can change the result for sessions no new event touched, and
/// each invalidates the whole cache: a correction or retraction (they target
/// other sessions), a change of ordering mode (`order::choose_mode` decides
/// it from the whole stream), and nothing else. The output is identical to
/// [`project`] over the same set of events, which the tests assert.
///
/// Duplicate event ids are ignored, so a caller may push the same event again
/// after it moved from the WAL into a segment.
#[derive(Debug, Default)]
pub struct IncrementalProjector {
    obs_by_session: HashMap<SessionId, Vec<Obs>>,
    meta: Vec<Obs>,
    keys: Vec<OrderKey>,
    seen: HashSet<EventId>,
    latest_at: Timestamp,
    mode: Option<OrderMode>,
    built: HashMap<SessionId, (SessionBuild, ProjectionStats)>,
    dirty: HashSet<SessionId>,
    all_dirty: bool,
}

impl IncrementalProjector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one event. Returns `false` when its id was already pushed.
    pub fn push(&mut self, ev: &Event) -> bool {
        if !self.seen.insert(ev.event_id) {
            return false;
        }
        let o = Obs::from_event(ev);
        self.keys.push(o.key);
        if o.at > self.latest_at {
            self.latest_at = o.at;
        }
        if o.meta.is_some() {
            self.meta.push(o);
            self.all_dirty = true;
        } else {
            self.dirty.insert(o.session_id);
            self.obs_by_session.entry(o.session_id).or_default().push(o);
        }
        true
    }

    /// Events pushed so far (duplicates excluded).
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Sessions that will be rebuilt by the next snapshot.
    pub fn pending_sessions(&self) -> usize {
        if self.all_dirty {
            self.obs_by_session.len()
        } else {
            self.dirty.len()
        }
    }

    /// The projection of everything pushed so far, judged against the
    /// stream's latest timestamp (as [`Projector::finish`]).
    pub fn snapshot(&mut self) -> Projection {
        self.snapshot_at(self.latest_at)
    }

    /// As [`Projector::finish_at`].
    pub fn snapshot_at(&mut self, now: Timestamp) -> Projection {
        let mode = order::choose_mode(self.keys.iter());
        if self.mode != Some(mode) {
            self.mode = Some(mode);
            self.all_dirty = true;
        }
        if self.all_dirty {
            self.built.clear();
            self.dirty = self.obs_by_session.keys().copied().collect();
            self.all_dirty = false;
        }

        let mut stats = ProjectionStats {
            events_seen: self.keys.len() as u64,
            out_of_order_events: self
                .keys
                .windows(2)
                .filter(|w| w[1].compare(&w[0], mode) == Ordering::Less)
                .count() as u64,
            ..Default::default()
        };

        // Corrections and retractions, in stream order.
        let mut meta: Vec<&Obs> = self.meta.iter().collect();
        meta.sort_by(|a, b| a.key.compare(&b.key, mode));
        let mut corrections = Vec::new();
        let mut retractions = Vec::new();
        for o in &meta {
            match o.kind {
                EventKind::Correction => corrections.push(meta::parse_correction(o)),
                EventKind::Retraction => retractions.push(meta::parse_retraction(o)),
                _ => {}
            }
        }
        stats.corrections_seen = corrections.len() as u64;
        stats.retractions_seen = retractions.len() as u64;
        let retracted_ids: RetractedSet = meta::retracted_set(&retractions);
        let has_retractions = !retractions.is_empty();

        // Rebuild what is dirty; sessions under a retraction are never
        // cached (they are rare and their bookkeeping mutates `retractions`).
        let mut retracted_builds = Vec::new();
        let mut session_ids: Vec<SessionId> = self.obs_by_session.keys().copied().collect();
        session_ids.sort();
        for sid in session_ids {
            let obs = &self.obs_by_session[&sid];
            if retracted_ids.contains_session(&sid) {
                self.built.remove(&sid);
                let mut sorted: Vec<&Obs> = obs.iter().collect();
                sorted.sort_by(|a, b| a.key.compare(&b.key, mode));
                for _ in &sorted {
                    meta::note_session_match(&mut retractions, sid);
                    stats.retracted_events += 1;
                }
                let mut discard = ProjectionStats::default();
                retracted_builds.extend(build_sessions(&sorted, &mut discard));
                continue;
            }
            if has_retractions {
                // Event-level retractions inside a live session change its
                // build, so a session with a retracted event is always dirty.
                for o in obs {
                    if retracted_ids.contains_event(&o.event_id) {
                        self.dirty.insert(sid);
                        break;
                    }
                }
            }
            if !self.dirty.contains(&sid) && self.built.contains_key(&sid) {
                continue;
            }
            let mut active: Vec<&Obs> = Vec::with_capacity(obs.len());
            for o in obs {
                if has_retractions && retracted_ids.contains_event(&o.event_id) {
                    continue;
                }
                active.push(o);
            }
            active.sort_by(|a, b| a.key.compare(&b.key, mode));
            let mut delta = ProjectionStats::default();
            let mut builds = build_sessions(&active, &mut delta);
            match builds.pop() {
                Some(b) if builds.is_empty() => {
                    self.built.insert(sid, (b, delta));
                }
                _ => {
                    // Every event of the session was retracted.
                    self.built.remove(&sid);
                }
            }
        }
        self.dirty.clear();
        if has_retractions {
            // Retraction notes and the retracted-event count are per
            // snapshot; count matches for every session, cached or not.
            for (sid, obs) in &self.obs_by_session {
                if retracted_ids.contains_session(sid) {
                    continue;
                }
                for o in obs {
                    if retracted_ids.contains_event(&o.event_id) {
                        meta::note_event_match(&mut retractions, o.event_id);
                        stats.retracted_events += 1;
                    }
                }
            }
        }

        let mut builds: Vec<SessionBuild> = Vec::with_capacity(self.built.len());
        for (b, delta) in self.built.values() {
            builds.push(b.clone());
            add_stats(&mut stats, delta);
        }
        builds.sort_by(|a, b| {
            (a.session.started_at, a.session.session_id)
                .cmp(&(b.session.started_at, b.session.session_id))
        });
        assemble(
            builds,
            retracted_builds,
            corrections,
            retractions,
            retracted_ids,
            stats,
            now,
        )
    }
}

/// Sum the per-session counters a session build produced into the
/// snapshot's totals. Global counters (events seen, ordering, corrections,
/// retractions) are computed once per snapshot and stay zero in a delta.
fn add_stats(total: &mut ProjectionStats, delta: &ProjectionStats) {
    total.events_seen += delta.events_seen;
    total.out_of_order_events += delta.out_of_order_events;
    total.unpaired_tool_starts += delta.unpaired_tool_starts;
    total.unpaired_tool_finishes += delta.unpaired_tool_finishes;
    total.fifo_pairings += delta.fifo_pairings;
    total.unknown_events += delta.unknown_events;
    total.injected_prompts += delta.injected_prompts;
    total.retracted_events += delta.retracted_events;
    total.corrections_seen += delta.corrections_seen;
    total.corrections_applied += delta.corrections_applied;
    total.retractions_seen += delta.retractions_seen;
}
