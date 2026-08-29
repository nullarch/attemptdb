//! The causal graph used by `TRACE` and exposed as the `edges` table.
//!
//! Projection edges are taken as-is. The query layer adds three kinds of
//! *derived* edges so that traversals reach the events that matter (RFC
//! 0004 §8 expects `caused` / `triggered` edges into attempts):
//!
//! - `triggered`: the prompt that opened a turn → every attempt in it,
//! - `caused`: the failing tool call end (or `TurnFailed`) → the attempt it
//!   ended,
//! - `blocked`: an uncleared pending-input signal → its session.
//!
//! Derived edges are marked (`edge_source = 'derived'`) and carry the event
//! they were derived from as evidence.

use crate::ids::readable;
use crate::tables::is_failed_status;
use attemptdb_core::{AttemptId, EventId, SpanId, TurnId};
use attemptdb_project::{Attempt, EdgeEndpoint, EdgeKind, Projection, ToolCall, Turn, TurnStatus};
use std::collections::{HashMap, HashSet, VecDeque};

/// Traversal direction for `TRACE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Toward causes: follow edges that point *at* the current node.
    Up,
    /// Toward effects: follow edges that leave the current node.
    Down,
    Both,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Up => "up",
            Direction::Down => "down",
            Direction::Both => "both",
        }
    }
}

#[derive(Clone, Debug)]
pub struct GraphEdge {
    pub from: EdgeEndpoint,
    pub to: EdgeEndpoint,
    pub kind: EdgeKind,
    pub evidence: Vec<EventId>,
    pub confidence: f32,
    pub derived: bool,
}

#[derive(Debug, Default)]
pub struct Graph {
    pub edges: Vec<GraphEdge>,
    incoming: HashMap<EdgeEndpoint, Vec<usize>>,
    outgoing: HashMap<EdgeEndpoint, Vec<usize>>,
}

/// One step of a traversal: the edge reached at `depth`.
#[derive(Clone, Copy, Debug)]
pub struct TraceStep {
    pub depth: usize,
    pub edge: usize,
}

pub fn endpoint_type(e: &EdgeEndpoint) -> &'static str {
    match e {
        EdgeEndpoint::Event(_) => "event",
        EdgeEndpoint::Span(_) => "tool_call",
        EdgeEndpoint::Turn(_) => "turn",
        EdgeEndpoint::Attempt(_) => "attempt",
        EdgeEndpoint::Session(_) => "session",
    }
}

pub fn endpoint_id(e: &EdgeEndpoint) -> String {
    match e {
        EdgeEndpoint::Event(id) => readable(id),
        EdgeEndpoint::Span(id) => readable(id),
        EdgeEndpoint::Turn(id) => readable(id),
        EdgeEndpoint::Attempt(id) => readable(id),
        EdgeEndpoint::Session(id) => readable(id),
    }
}

/// Edge kinds a `TRACE` follows. `evidence_for` is excluded: it links every
/// evidence event to its attempt and would swamp the causal story.
pub fn is_causal(kind: EdgeKind) -> bool {
    !matches!(kind, EdgeKind::EvidenceFor)
}

/// The event that ended a failed attempt: the last failing/denied tool call
/// end in the attempt, else the `TurnFailed` stop event of its turn.
pub fn failing_event(
    a: &Attempt,
    calls: &HashMap<SpanId, &ToolCall>,
    turn: Option<&Turn>,
) -> Option<EventId> {
    for id in a.tool_call_ids.iter().rev() {
        if let Some(c) = calls.get(id)
            && c.outcome
                .as_ref()
                .is_some_and(|o| is_failed_status(o.status))
            && let Some(end) = c.end_event_id
        {
            return Some(end);
        }
    }
    match turn {
        Some(t) if t.status == TurnStatus::Failed => t.stop_event_id,
        _ => None,
    }
}

impl Graph {
    pub fn build(p: &Projection) -> Self {
        let attempt_conf: HashMap<AttemptId, f32> = p
            .attempts
            .iter()
            .map(|a| (a.attempt_id, a.confidence))
            .collect();
        let handoff_conf: HashMap<(EdgeEndpoint, EdgeEndpoint), f32> = p
            .handoffs
            .iter()
            .map(|h| {
                (
                    (
                        EdgeEndpoint::Session(h.from_session),
                        EdgeEndpoint::Session(h.to_session),
                    ),
                    h.confidence,
                )
            })
            .collect();
        let mut edges: Vec<GraphEdge> = Vec::with_capacity(p.edges.len());
        for e in &p.edges {
            let confidence = match (e.kind, &e.from) {
                (EdgeKind::Superseded, EdgeEndpoint::Attempt(id)) => {
                    attempt_conf.get(id).copied().unwrap_or(1.0)
                }
                (EdgeKind::HandedOff, _) => {
                    handoff_conf.get(&(e.from, e.to)).copied().unwrap_or(1.0)
                }
                _ => 1.0,
            };
            edges.push(GraphEdge {
                from: e.from,
                to: e.to,
                kind: e.kind,
                evidence: e.evidence.clone(),
                confidence,
                derived: false,
            });
        }

        let turns: HashMap<TurnId, &Turn> = p.turns.iter().map(|t| (t.turn_id, t)).collect();
        let calls: HashMap<SpanId, &ToolCall> =
            p.tool_calls.iter().map(|c| (c.tool_call_id, c)).collect();
        for a in &p.attempts {
            let turn = turns.get(&a.turn_id).copied();
            if let Some(prompt) = turn.and_then(|t| t.prompt_event_id) {
                edges.push(GraphEdge {
                    from: EdgeEndpoint::Event(prompt),
                    to: EdgeEndpoint::Attempt(a.attempt_id),
                    kind: EdgeKind::Triggered,
                    evidence: vec![prompt],
                    confidence: 1.0,
                    derived: true,
                });
            }
            if a.outcome.is_failure()
                && let Some(f) = failing_event(a, &calls, turn)
            {
                edges.push(GraphEdge {
                    from: EdgeEndpoint::Event(f),
                    to: EdgeEndpoint::Attempt(a.attempt_id),
                    kind: EdgeKind::Caused,
                    evidence: vec![f],
                    confidence: a.confidence,
                    derived: true,
                });
            }
        }
        for g in p.signals.iter().filter(|g| g.cleared_at.is_none()) {
            edges.push(GraphEdge {
                from: EdgeEndpoint::Event(g.event_id),
                to: EdgeEndpoint::Session(g.session_id),
                kind: EdgeKind::Blocked,
                evidence: vec![g.event_id],
                confidence: 1.0,
                derived: true,
            });
        }

        let mut incoming: HashMap<EdgeEndpoint, Vec<usize>> = HashMap::new();
        let mut outgoing: HashMap<EdgeEndpoint, Vec<usize>> = HashMap::new();
        for (i, e) in edges.iter().enumerate() {
            incoming.entry(e.to).or_default().push(i);
            outgoing.entry(e.from).or_default().push(i);
        }
        Self {
            edges,
            incoming,
            outgoing,
        }
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Breadth-first traversal from `start` over causal edges up to
    /// `max_depth`. Returns the steps in traversal order and whether the
    /// depth limit cut the walk short.
    pub fn trace(
        &self,
        start: EdgeEndpoint,
        max_depth: usize,
        direction: Direction,
    ) -> (Vec<TraceStep>, bool) {
        let mut steps = Vec::new();
        let mut visited: HashSet<EdgeEndpoint> = HashSet::from([start]);
        let mut emitted: HashSet<usize> = HashSet::new();
        let mut queue: VecDeque<(EdgeEndpoint, usize)> = VecDeque::from([(start, 0)]);
        let mut truncated = false;
        while let Some((node, depth)) = queue.pop_front() {
            let mut nexts: Vec<(usize, EdgeEndpoint)> = Vec::new();
            if matches!(direction, Direction::Up | Direction::Both)
                && let Some(ids) = self.incoming.get(&node)
            {
                nexts.extend(
                    ids.iter()
                        .filter(|&&i| is_causal(self.edges[i].kind))
                        .map(|&i| (i, self.edges[i].from)),
                );
            }
            if matches!(direction, Direction::Down | Direction::Both)
                && let Some(ids) = self.outgoing.get(&node)
            {
                nexts.extend(
                    ids.iter()
                        .filter(|&&i| is_causal(self.edges[i].kind))
                        .map(|&i| (i, self.edges[i].to)),
                );
            }
            if depth >= max_depth {
                if nexts.iter().any(|(i, _)| !emitted.contains(i)) {
                    truncated = true;
                }
                continue;
            }
            for (i, next) in nexts {
                if emitted.insert(i) {
                    steps.push(TraceStep {
                        depth: depth + 1,
                        edge: i,
                    });
                }
                if visited.insert(next) {
                    queue.push_back((next, depth + 1));
                }
            }
        }
        (steps, truncated)
    }
}
