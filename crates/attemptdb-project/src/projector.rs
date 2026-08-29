//! The projector: consumes events and produces a [`Projection`].
//!
//! [`Projector::push`] records a compact observation per event (no content
//! beyond the prompt text); [`Projector::finish`] sorts the observations
//! defensively (see [`crate::order`]) and runs the whole pipeline. Doing the
//! reduction in `finish` keeps the output a pure function of the event set,
//! whatever order the caller pushed in.
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
use crate::handoff::{self, HandoffInput, PathTouch};
use crate::model::{
    AlgorithmVersion, Attempt, CausalEdge, CoverageGrade, EdgeEndpoint, EdgeKind, Projection,
    ProjectionStats, Session, Signal, ToolCall, Turn, TurnStatus,
};
use crate::order::{self, OrderKey};
use attemptdb_core::event::Provider;
use attemptdb_core::{
    AgentId, Event, EventId, EventKind, Outcome, OutcomeStatus, PortablePath, ProjectId, SessionId,
    SpanId, Timestamp, ToolCategory, ToolRef, TurnId,
};
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};

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

/// The subset of an event the projector needs.
#[derive(Clone, Debug)]
struct Obs {
    key: OrderKey,
    event_id: EventId,
    session_id: SessionId,
    provider: Provider,
    provider_session_id: String,
    project_id: ProjectId,
    project_name: String,
    kind: EventKind,
    at: Timestamp,
    agent_id: AgentId,
    tool: Option<ToolRef>,
    outcome: Option<Outcome>,
    duration_ms: Option<u64>,
    paths: Vec<PortablePath>,
    prompt: Option<String>,
    prompt_chars: Option<u64>,
    /// Kind-specific metadata: start source, end reason, notification type,
    /// or turn failure class.
    note: Option<String>,
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
            EventKind::ToolCallStarted | EventKind::ToolCallFinished | EventKind::ToolCallFailed
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

    /// Sort the observations and build the projection.
    pub fn finish(self) -> Projection {
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

        let mut builds: Vec<SessionBuild> = Vec::new();
        let mut by_session: HashMap<SessionId, usize> = HashMap::new();
        for o in &obs {
            let idx = *by_session.entry(o.session_id).or_insert_with(|| {
                builds.push(SessionBuild::new(o));
                builds.len() - 1
            });
            builds[idx].apply(o, &mut stats);
        }
        drop(obs);

        for b in &mut builds {
            b.finalize(&mut stats);
        }
        builds.sort_by(|a, b| {
            (a.session.started_at, a.session.session_id)
                .cmp(&(b.session.started_at, b.session.session_id))
        });

        let handoff_inputs: Vec<HandoffInput> =
            builds.iter().map(SessionBuild::handoff_input).collect();
        let handoffs = handoff::detect(&handoff_inputs);

        let mut projection = Projection {
            algorithm_version: AlgorithmVersion::current(),
            sessions: Vec::with_capacity(builds.len()),
            turns: Vec::new(),
            tool_calls: Vec::new(),
            attempts: Vec::new(),
            handoffs: Vec::new(),
            edges: Vec::new(),
            signals: Vec::new(),
            stats: ProjectionStats::default(),
        };

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
            projection.signals.extend(b.signals);
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
        projection.stats = stats;
        projection
    }
}

/// Project a complete event stream. Input is sorted defensively.
pub fn project<'a>(events: impl IntoIterator<Item = &'a Event>) -> Projection {
    let mut p = Projector::new();
    for ev in events {
        p.push(ev);
    }
    p.finish()
}

#[derive(Clone, Copy, Debug)]
struct CallMeta {
    /// Index into `SessionBuild::turns`.
    turn: usize,
    pairing: Pairing,
}

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
        }
    }

    fn apply(&mut self, o: &Obs, stats: &mut ProjectionStats) {
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
        match matched {
            Some((i, pairing)) => {
                let call = &mut self.calls[i];
                call.finished_at = Some(o.at);
                call.end_event_id = Some(o.event_id);
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
                self.call_meta[i].pairing = pairing;
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
                });
                self.call_meta.push(CallMeta {
                    turn: ti,
                    pairing: Pairing::LoneFinish,
                });
                self.turns[ti].tool_call_ids.push(span);
                self.session.tool_call_count += 1;
                stats.unpaired_tool_finishes += 1;
            }
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
