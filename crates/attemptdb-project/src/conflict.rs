//! Work conflicts: two open work units editing the same files at the same
//! time (Tier 1, `conflict-v0`).
//!
//! The one inference that only exists where sessions from different
//! agents — and, on a server, different devices — meet in one projection.
//! It is the "work conflict" a team console raises before git ever sees a
//! merge conflict: not a judgement about intent, a statement that two
//! actors are changing the same file in overlapping windows and neither
//! has committed since.
//!
//! Rule (all of):
//!
//! 1. Two work units of the same project, both open, with no session in
//!    common (one agent editing in two units is not a conflict).
//! 2. At least one path both units edited (file edit/write tool calls).
//! 3. On that path, the two units' edit windows — first to last edit —
//!    overlap, or lie within [`WINDOW`] of each other.
//! 4. Confidence: 0.7 when both sides are uncommitted on the path and the
//!    windows overlap; 0.5 when they merely lie within the window or one
//!    side has committed since its last edit. Evidence is the edit events
//!    on each side (up to [`EVIDENCE_PER_SIDE`] per side per path).
//!
//! What it cannot see: edits outside the hook surface, a commit made by a
//! tool the hooks do not classify as `git commit`, and clock skew between
//! devices (windows are compared on `observed_at`).

use crate::model::{
    Commit, Conflict, ConflictPath, Projection, ToolCall, WorkUnit, WorkUnitStatus,
};
use attemptdb_core::{ConflictId, EventId, SessionId, Timestamp, ToolCategory, TurnId};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Edit windows on one path that lie this close still count as concurrent.
pub const WINDOW: i64 = 2 * 60 * 60 * 1_000_000;
const EVIDENCE_PER_SIDE: usize = 3;
pub const ALGORITHM_VERSION: &str = "conflict-v0";

/// What one unit did to one path.
#[derive(Debug, Default)]
struct PathEdits {
    first_at: Option<Timestamp>,
    last_at: Option<Timestamp>,
    added: u64,
    removed: u64,
    evidence: Vec<EventId>,
}

fn is_edit(c: &ToolCall) -> bool {
    matches!(
        c.tool.category,
        ToolCategory::FileEdit | ToolCategory::FileWrite | ToolCategory::Notebook
    )
}

fn call_at(c: &ToolCall) -> Option<Timestamp> {
    c.finished_at.or(c.started_at)
}

/// Per unit, per path: the edit window, size and evidence.
fn edits_by_unit(p: &Projection) -> Vec<BTreeMap<String, PathEdits>> {
    let mut unit_of_turn: HashMap<TurnId, usize> = HashMap::new();
    for (i, u) in p.work_units.iter().enumerate() {
        for t in &u.turns {
            unit_of_turn.insert(*t, i);
        }
    }
    let mut out: Vec<BTreeMap<String, PathEdits>> =
        (0..p.work_units.len()).map(|_| BTreeMap::new()).collect();
    for c in &p.tool_calls {
        if !is_edit(c) {
            continue;
        }
        let Some(&i) = c.turn_id.as_ref().and_then(|t| unit_of_turn.get(t)) else {
            continue;
        };
        let Some(at) = call_at(c) else { continue };
        for path in &c.paths {
            let e = out[i].entry(path.logical.clone()).or_default();
            e.first_at = Some(e.first_at.map_or(at, |f| f.min(at)));
            e.last_at = Some(e.last_at.map_or(at, |l| l.max(at)));
            e.added += c.lines_added.unwrap_or(0);
            e.removed += c.lines_removed.unwrap_or(0);
            if e.evidence.len() < EVIDENCE_PER_SIDE
                && let Some(id) = c.end_event_id.or(c.start_event_id)
            {
                e.evidence.push(id);
            }
        }
    }
    out
}

/// Whether the unit committed (a `git commit` call in one of its sessions)
/// at or after `at`.
fn committed_since(commits: &[Commit], sessions: &HashSet<SessionId>, at: Timestamp) -> bool {
    commits
        .iter()
        .any(|c| sessions.contains(&c.session_id) && c.at >= at)
}

/// Every conflict between the projection's open work units, ordered by
/// the earlier unit's start then id.
pub fn detect(p: &Projection) -> Vec<Conflict> {
    let edits = edits_by_unit(p);
    let sessions: Vec<HashSet<SessionId>> = p
        .work_units
        .iter()
        .map(|u| u.sessions.iter().copied().collect())
        .collect();
    let mut out = Vec::new();
    let units: Vec<(usize, &WorkUnit)> = p
        .work_units
        .iter()
        .enumerate()
        .filter(|(_, u)| u.status == WorkUnitStatus::Open)
        .collect();
    for (ai, (i, a)) in units.iter().enumerate() {
        for (j, b) in units.iter().skip(ai + 1).map(|(j, b)| (*j, *b)) {
            if a.project_id != b.project_id || !sessions[*i].is_disjoint(&sessions[j]) {
                continue;
            }
            let (first, second, fi, si) =
                if (a.started_at, a.work_unit_id) <= (b.started_at, b.work_unit_id) {
                    (*a, b, *i, j)
                } else {
                    (b, *a, j, *i)
                };
            let mut paths = Vec::new();
            let mut evidence = Vec::new();
            let mut any_overlap = false;
            let mut all_uncommitted = true;
            let (mut started, mut updated): (Option<Timestamp>, Option<Timestamp>) = (None, None);
            for (path, fe) in &edits[fi] {
                let Some(se) = edits[si].get(path) else {
                    continue;
                };
                let (Some(f0), Some(f1), Some(s0), Some(s1)) =
                    (fe.first_at, fe.last_at, se.first_at, se.last_at)
                else {
                    continue;
                };
                let overlap = f0 <= s1 && s0 <= f1;
                let gap = if overlap {
                    0
                } else {
                    (s0.as_micros() - f1.as_micros()).max(f0.as_micros() - s1.as_micros())
                };
                if !overlap && gap > WINDOW {
                    continue;
                }
                let first_committed = committed_since(&p.commits, &sessions[fi], f1);
                let second_committed = committed_since(&p.commits, &sessions[si], s1);
                any_overlap |= overlap;
                all_uncommitted &= !first_committed && !second_committed;
                let lo = f0.min(s0);
                let hi = f1.max(s1);
                started = Some(started.map_or(lo, |x| x.min(lo)));
                updated = Some(updated.map_or(hi, |x| x.max(hi)));
                for id in fe.evidence.iter().chain(&se.evidence) {
                    if !evidence.contains(id) {
                        evidence.push(*id);
                    }
                }
                paths.push(ConflictPath {
                    path: path.clone(),
                    first_added: fe.added,
                    first_removed: fe.removed,
                    second_added: se.added,
                    second_removed: se.removed,
                    overlapping: overlap,
                    first_committed,
                    second_committed,
                });
            }
            if paths.is_empty() {
                continue;
            }
            let confidence = if any_overlap && all_uncommitted {
                0.7
            } else {
                0.5
            };
            out.push(Conflict {
                conflict_id: ConflictId::derive(&[
                    &first.work_unit_id.to_string(),
                    &second.work_unit_id.to_string(),
                ]),
                project_id: first.project_id,
                first: first.work_unit_id,
                second: second.work_unit_id,
                first_started_at: first.started_at,
                second_started_at: second.started_at,
                started_at: started.unwrap_or(first.started_at),
                updated_at: updated.unwrap_or(second.updated_at),
                paths,
                evidence,
                confidence,
                algorithm_version: ALGORITHM_VERSION.to_string(),
            });
        }
    }
    out.sort_by(|a, b| {
        (a.first_started_at, a.conflict_id).cmp(&(b.first_started_at, b.conflict_id))
    });
    out
}
