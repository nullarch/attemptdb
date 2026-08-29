//! Cross-provider handoff detection.
//!
//! Rule (v0): session B received a handoff from session A when both belong to
//! the same project, use *different* providers, A had no activity after B
//! started, and either
//!
//! - B started within 30 minutes of A's last activity (or A's end, whichever
//!   is later but still before B) and B touched at least one path A touched
//!   (confidence `0.8`), or
//! - B started within 5 minutes regardless of paths (confidence `0.5`).
//!
//! Each receiving session gets at most one handoff: the candidate with shared
//! paths, then the smallest gap, then the smallest session id. Same-provider
//! successions are continuations, not handoffs, and are ignored.

use crate::model::Handoff;
use attemptdb_core::event::Provider;
use attemptdb_core::{EventId, ProjectId, SessionId, Timestamp};
use std::collections::BTreeMap;

pub(crate) const SHARED_PATH_WINDOW_US: i64 = 30 * 60 * 1_000_000;
pub(crate) const QUICK_WINDOW_US: i64 = 5 * 60 * 1_000_000;

/// First and last tool event in a session that reported a given path.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PathTouch {
    pub first: EventId,
    pub last: EventId,
}

pub(crate) struct HandoffInput {
    pub session_id: SessionId,
    pub provider: Provider,
    pub project_id: ProjectId,
    pub started_at: Timestamp,
    pub ended_at: Option<Timestamp>,
    /// Last event that was not `SessionEnded`.
    pub last_activity_at: Timestamp,
    pub first_event_id: EventId,
    pub last_event_id: EventId,
    pub paths: BTreeMap<String, PathTouch>,
    /// A session with neither prompts nor tool calls (capture tests, stray
    /// lifecycle events) can neither give nor receive work.
    pub active: bool,
}

pub(crate) fn detect(inputs: &[HandoffInput]) -> Vec<Handoff> {
    let mut out = Vec::new();
    for b in inputs.iter().filter(|b| b.active) {
        let mut best: Option<((u8, i64, SessionId), Handoff)> = None;
        for a in inputs.iter().filter(|a| a.active) {
            if a.session_id == b.session_id
                || a.project_id != b.project_id
                || a.provider == b.provider
                || a.last_activity_at > b.started_at
            {
                continue;
            }
            let a_last = match a.ended_at {
                Some(e) if e <= b.started_at => e.max(a.last_activity_at),
                _ => a.last_activity_at,
            };
            let gap = b.started_at.as_micros() - a_last.as_micros();
            if gap < 0 {
                continue;
            }
            let shared: Vec<&String> = a
                .paths
                .keys()
                .filter(|p| b.paths.contains_key(p.as_str()))
                .collect();
            let confidence = if !shared.is_empty() && gap <= SHARED_PATH_WINDOW_US {
                0.8
            } else if gap <= QUICK_WINDOW_US {
                0.5
            } else {
                continue;
            };
            let mut evidence = vec![a.last_event_id, b.first_event_id];
            if let Some(p) = shared.first() {
                evidence.push(a.paths[p.as_str()].last);
                evidence.push(b.paths[p.as_str()].first);
            }
            let mut dedup: Vec<EventId> = Vec::new();
            for e in evidence {
                if !dedup.contains(&e) {
                    dedup.push(e);
                }
            }
            let handoff = Handoff {
                from_session: a.session_id,
                to_session: b.session_id,
                from_provider: a.provider.clone(),
                to_provider: b.provider.clone(),
                project_id: b.project_id,
                at: b.started_at,
                gap_ms: (gap / 1_000) as u64,
                shared_paths: shared.iter().map(|s| (*s).clone()).collect(),
                evidence: dedup,
                confidence,
            };
            let rank = (u8::from(shared.is_empty()), gap, a.session_id);
            if best.as_ref().is_none_or(|(r, _)| rank < *r) {
                best = Some((rank, handoff));
            }
        }
        if let Some((_, h)) = best {
            out.push(h);
        }
    }
    out.sort_by(|x, y| {
        (x.at, x.to_session, x.from_session).cmp(&(y.at, y.to_session, y.from_session))
    });
    out
}
