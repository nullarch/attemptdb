//! `GET /v1/live` — "is this user coding right now", without the engine.
//!
//! The app's coding-state loop (VibeMon's `useCodingState`) asks one thing
//! every few seconds: the newest event's time and kind. Answering that
//! through the read view means opening the tenant, and on a small machine
//! where a handful of tenants stay resident, a poll from an evicted tenant
//! is a database open, a segment decode and a projection — 300 ms for a
//! 60 k-event user, 700 ms for the largest one, every five seconds.
//!
//! [`LiveState`] is a few hundred bytes per tenant, kept for every tenant
//! the process has seen regardless of what the registry evicts, updated
//! from the events an ingest accepted (no decode), and seeded from the
//! stream facts the first time a tenant is asked about after a start.
//! Reads take a mutex and format JSON.

use crate::AppState;
use crate::shape as sh;
use crate::tenants::TenantId;
use attemptdb_core::{Event, EventKind, ProjectId, SessionId, Timestamp};
use attemptdb_query::StreamFacts;
use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Sessions without an event for this long leave the live map: they are
/// not "active" by any window a client would ask for.
const SESSION_RETENTION: i64 = 24 * 60 * 60 * 1_000_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LastEvent {
    pub at: Timestamp,
    pub kind: EventKind,
    pub provider: String,
    pub session_id: SessionId,
    pub project_id: ProjectId,
    pub tool: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionLive {
    pub provider: String,
    pub project_id: ProjectId,
    pub last_event_at: Timestamp,
    pub last_kind: EventKind,
}

/// One tenant's live facts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LiveState {
    pub events: u64,
    pub last_event: Option<LastEvent>,
    pub sessions: HashMap<SessionId, SessionLive>,
    /// Server time of the last change.
    pub updated_at: Option<Timestamp>,
}

impl LiveState {
    /// Seed from the facts of everything stored (once per tenant per
    /// process, when the first `/v1/live` arrives before any ingest).
    pub fn from_facts(f: &StreamFacts) -> Self {
        let mut s = Self {
            events: f.events,
            last_event: f.last_event.as_ref().map(|e| LastEvent {
                at: e.at,
                kind: e.kind,
                provider: e.provider.clone(),
                session_id: e.session_id,
                project_id: e.project_id,
                tool: e.tool.clone(),
            }),
            sessions: HashMap::new(),
            updated_at: Some(Timestamp::now()),
        };
        for (sid, sf) in &f.sessions {
            if let (Some(at), Some(kind)) = (sf.last_event_at, sf.last_kind) {
                s.sessions.insert(
                    *sid,
                    SessionLive {
                        provider: sf.provider.clone(),
                        project_id: sf.project_id,
                        last_event_at: at,
                        last_kind: kind,
                    },
                );
            }
        }
        s.prune();
        s
    }

    /// Fold in events an ingest accepted. Capture tests do not count as
    /// activity; a re-sent old event cannot move anything backwards.
    pub fn absorb(&mut self, events: &[Event]) {
        for ev in events {
            self.events += 1;
            if ev.kind == EventKind::CaptureTest {
                continue;
            }
            if self
                .last_event
                .as_ref()
                .is_none_or(|l| ev.observed_at >= l.at)
            {
                self.last_event = Some(LastEvent {
                    at: ev.observed_at,
                    kind: ev.kind,
                    provider: ev.provider.as_str().to_string(),
                    session_id: ev.session_id,
                    project_id: ev.project.project_id,
                    tool: ev.tool.as_ref().map(|t| t.name.clone()),
                });
            }
            let entry = self
                .sessions
                .entry(ev.session_id)
                .or_insert_with(|| SessionLive {
                    provider: ev.provider.as_str().to_string(),
                    project_id: ev.project.project_id,
                    last_event_at: ev.observed_at,
                    last_kind: ev.kind,
                });
            if ev.observed_at >= entry.last_event_at {
                entry.last_event_at = ev.observed_at;
                entry.last_kind = ev.kind;
            }
        }
        self.updated_at = Some(Timestamp::now());
        self.prune();
    }

    /// The live facts of one batch, to be merged after the batch is
    /// durable: computed before the ingest consumes the events, and small
    /// (the batch's newest event and one entry per session it touched).
    pub fn from_events(events: &[Event]) -> Self {
        let mut s = Self::default();
        s.absorb(events);
        s
    }

    /// Fold another live state in (a batch's delta, or a seed that raced).
    pub fn merge(&mut self, other: &LiveState) {
        self.events += other.events;
        if other
            .last_event
            .as_ref()
            .is_some_and(|o| self.last_event.as_ref().is_none_or(|l| o.at >= l.at))
        {
            self.last_event = other.last_event.clone();
        }
        for (sid, theirs) in &other.sessions {
            match self.sessions.get_mut(sid) {
                Some(mine) if theirs.last_event_at < mine.last_event_at => {}
                Some(mine) => {
                    mine.last_event_at = theirs.last_event_at;
                    mine.last_kind = theirs.last_kind;
                }
                None => {
                    self.sessions.insert(*sid, theirs.clone());
                }
            }
        }
        self.updated_at = Some(Timestamp::now());
        self.prune();
    }

    fn prune(&mut self) {
        let Some(newest) = self.last_event.as_ref().map(|l| l.at) else {
            return;
        };
        self.sessions
            .retain(|_, s| newest.as_micros() - s.last_event_at.as_micros() <= SESSION_RETENTION);
    }

    /// Sessions with an event in the last `window` microseconds before
    /// `now`, newest first.
    pub fn active(&self, now: Timestamp, window: i64) -> Vec<(SessionId, &SessionLive)> {
        let mut out: Vec<_> = self
            .sessions
            .iter()
            .filter(|(_, s)| now.as_micros() - s.last_event_at.as_micros() <= window)
            .map(|(id, s)| (*id, s))
            .collect();
        out.sort_by(|a, b| {
            b.1.last_event_at
                .cmp(&a.1.last_event_at)
                .then(a.0.cmp(&b.0))
        });
        out
    }
}

/// The live map: every tenant this process has ingested for or been asked
/// about. Never evicted; a few hundred bytes per tenant.
#[derive(Default)]
pub struct LiveMap {
    inner: Mutex<HashMap<TenantId, LiveState>>,
}

impl LiveMap {
    /// After an ingest: fold a batch's delta in. A tenant not yet in the
    /// map is left for the next read to seed (its stored history is not in
    /// this batch).
    pub fn merge(&self, tenant: &TenantId, delta: &LiveState) {
        if let Ok(mut m) = self.inner.lock()
            && let Some(s) = m.get_mut(tenant)
        {
            s.merge(delta);
        }
    }

    pub fn get(&self, tenant: &TenantId) -> Option<LiveState> {
        self.inner.lock().ok()?.get(tenant).cloned()
    }

    /// Seed a tenant that the map does not hold yet; a seed that raced an
    /// absorb keeps the newer of the two.
    pub fn seed(&self, tenant: &TenantId, state: LiveState) -> LiveState {
        let Ok(mut m) = self.inner.lock() else {
            return state;
        };
        let entry = m.entry(tenant.clone()).or_insert(state);
        entry.clone()
    }
}

/// `GET /v1/live[?window=<seconds>]` — the newest event and the sessions
/// active within `window` (default 600 s). Reader or admin key.
pub async fn live(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let principal = match crate::read::reader_principal(&state, &headers) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let window_secs: i64 = match q.get("window").map(|s| s.trim()).filter(|s| !s.is_empty()) {
        None => 600,
        Some(s) => match s.parse::<i64>() {
            Ok(n) if n > 0 => n,
            _ => {
                return crate::read::error_response(
                    StatusCode::BAD_REQUEST,
                    format!("window={s:?} is not a positive number of seconds"),
                );
            }
        },
    };
    let tenant = principal.tenant.clone();
    let live = match state.live.get(&tenant) {
        Some(l) => l,
        None => {
            // First ask for this tenant since the process started: seed from
            // the stored facts (one view build), then never again.
            match crate::read::load_view(&state, &principal).await {
                Ok(view) => state
                    .live
                    .seed(&tenant, LiveState::from_facts(view.engine.facts())),
                Err(r) => return *r,
            }
        }
    };
    let now = Timestamp::now();
    let active: Vec<_> = live
        .active(now, window_secs * 1_000_000)
        .into_iter()
        .map(|(sid, s)| {
            json!({
                "session_id": sh::id(&sid),
                "provider": s.provider,
                "project_id": sh::id(&s.project_id),
                "last_event_at": sh::ts(s.last_event_at),
                "last_kind": s.last_kind.as_str(),
                "idle_ms": (now.as_micros() - s.last_event_at.as_micros()).max(0) / 1000,
            })
        })
        .collect();
    let last = live.last_event.as_ref().map(|l| {
        json!({
            "at": sh::ts(l.at),
            "kind": l.kind.as_str(),
            "provider": l.provider,
            "session_id": sh::id(&l.session_id),
            "project_id": sh::id(&l.project_id),
            "tool": l.tool,
            "idle_ms": (now.as_micros() - l.at.as_micros()).max(0) / 1000,
        })
    });
    Json(json!({
        "tenant": tenant.as_str(),
        "server_time": sh::ts(now),
        "events": live.events,
        "last_event": last,
        "window_secs": window_secs,
        "active_sessions": active,
        "note": "facts only: the newest event and recently active sessions; no inference",
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use attemptdb_core::event::Provider;
    use attemptdb_core::{CaptureMode, DeviceId, ProjectRef};

    fn ev(kind: EventKind, session: &str, at: i64) -> Event {
        let dev = DeviceId::derive(&["live-test"]);
        let mut e = Event::new(
            dev,
            Provider::ClaudeCode,
            "x",
            kind,
            ProjectRef::derive("/home/dev/example/project", None, &dev),
            session.to_string(),
            CaptureMode::MetadataOnly,
            "live-test/0",
        );
        e.observed_at = Timestamp::from_micros(at);
        e
    }

    #[test]
    fn absorb_keeps_the_newest_and_never_moves_backwards() {
        let mut s = LiveState::default();
        s.absorb(&[ev(EventKind::PromptSubmitted, "a", 1_000_000)]);
        s.absorb(&[ev(EventKind::ToolCallFinished, "b", 3_000_000)]);
        // A re-sent old event: counted, but not "last".
        s.absorb(&[ev(EventKind::ToolCallFailed, "a", 2_000_000)]);
        let last = s.last_event.as_ref().unwrap();
        assert_eq!(last.kind, EventKind::ToolCallFinished);
        assert_eq!(last.at.as_micros(), 3_000_000);
        assert_eq!(s.events, 3);
        assert_eq!(s.sessions.len(), 2);
        // Capture tests are not activity.
        s.absorb(&[ev(EventKind::CaptureTest, "c", 9_000_000)]);
        assert_eq!(s.last_event.as_ref().unwrap().at.as_micros(), 3_000_000);
        assert_eq!(s.sessions.len(), 2);
        let active = s.active(Timestamp::from_micros(3_500_000), 1_000_000);
        assert_eq!(active.len(), 1, "only b is within the last second");
        assert_eq!(active[0].1.last_kind, EventKind::ToolCallFinished);
    }

    #[test]
    fn sessions_older_than_a_day_are_pruned() {
        let mut s = LiveState::default();
        s.absorb(&[ev(EventKind::PromptSubmitted, "old", 0)]);
        s.absorb(&[ev(EventKind::PromptSubmitted, "new", 2 * SESSION_RETENTION)]);
        assert_eq!(s.sessions.len(), 1);
        assert!(
            s.sessions
                .values()
                .all(|x| x.last_event_at.as_micros() == 2 * SESSION_RETENTION)
        );
    }

    #[test]
    fn map_absorbs_only_seeded_tenants() {
        let map = LiveMap::default();
        let t = TenantId::parse("acme").unwrap();
        let delta = LiveState::from_events(&[ev(EventKind::PromptSubmitted, "a", 1)]);
        map.merge(&t, &delta);
        assert!(map.get(&t).is_none(), "an unseeded tenant stays unknown");
        map.seed(&t, LiveState::default());
        map.merge(&t, &delta);
        assert_eq!(map.get(&t).unwrap().events, 1);
        assert_eq!(map.get(&t).unwrap().sessions.len(), 1);
    }
}
