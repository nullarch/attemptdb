//! Evidence lookups and web-originated corrections.
//!
//! Two things a console needs that the read API did not have:
//!
//! - `GET /v1/events/{id}` — an evidence card links to its event. The
//!   manifest's per-segment id range says which segment to decode; only
//!   that one is read. Metadata only: the event's content stayed on the
//!   device that observed it.
//! - `POST /v1/corrections` — "this is not a problem", "fix the title",
//!   "intended division of work". A reader or admin key writes a
//!   Correction (or a Retraction) event under the server's own writer
//!   identity, with the acting user recorded as a provider extension
//!   attribute. The observed facts stay; the projection re-reads them
//!   through the corrections table exactly as it does for a device's own
//!   `attempt correct`.

use crate::AppState;
use crate::read::{error_response, load_view, reader_principal};
use crate::shape as sh;
use attemptdb_core::event::Provider;
use attemptdb_core::{AgentId, AttemptId, Event, EventId, EventKind, SessionId, TurnId};
use attemptdb_project::{
    CORRECTABLE_OUTCOMES, CorrectionType, RetractionReason, RetractionTargetType,
};
use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::sync::Arc;

/// `GET /v1/events/{id}` — one stored event by id (`ev_…` or a bare
/// uuid), as stored: metadata, never content.
pub async fn event_by_id(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    let principal = match reader_principal(&state, &headers) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let Ok(event_id) = id.parse::<EventId>() else {
        return error_response(
            StatusCode::BAD_REQUEST,
            format!("{id:?} is not an event id (ev_… or a uuid)"),
        );
    };
    let view = match load_view(&state, &principal).await {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let tenant = principal.tenant.clone();
    let found = tokio::task::spawn_blocking(move || find_event(&view, event_id)).await;
    match found {
        Ok(Some(ev)) => Json(json!({
            "tenant": tenant.as_str(),
            "event": serde_json::to_value(&ev).unwrap_or(Value::Null),
            "note": "as stored on the server: metadata only; the observing device holds the content",
        }))
        .into_response(),
        Ok(None) => error_response(StatusCode::NOT_FOUND, format!("no event {event_id} in this tenant")),
        Err(e) => error_response(StatusCode::SERVICE_UNAVAILABLE, format!("lookup failed: {e}")),
    }
}

/// Decode only the segments whose id range covers `id` (one, for
/// time-ordered ids), then the WAL.
fn find_event(view: &crate::engine::TenantView, id: EventId) -> Option<Event> {
    let reader = view.refreshed.reader();
    for seg in &view.refreshed.segments {
        if seg.meta.min_event_id <= id
            && id <= seg.meta.max_event_id
            && let Ok(events) = seg.decode(Some(&reader))
            && let Some(ev) = events.into_iter().find(|e| e.event_id == id)
        {
            return Some(ev);
        }
    }
    view.refreshed
        .memtable
        .iter()
        .find(|e| e.event_id == id)
        .cloned()
}

/// A correction from the web: what the CLI's `attempt correct` /
/// `attempt retract` take, minus the free-text note's content when the
/// server's ceiling forbids it (its length is kept).
#[derive(Debug, Deserialize)]
pub struct CorrectionRequest {
    /// `att_…`, `trn_…` or `ses_…`.
    pub target: String,
    /// `attempt_outcome`, `attempt_note`, `turn_objective`, or
    /// `retract_session`.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub outcome: Option<String>,
    #[serde(default)]
    pub failure_class: Option<String>,
    /// The note, the new objective, or the retraction's note.
    #[serde(default)]
    pub note: Option<String>,
    /// Retraction reason (`mistake`, `benchmark`, `duplicate`, `privacy`,
    /// `revoked`, `other`); default `mistake`.
    #[serde(default)]
    pub reason: Option<String>,
}

/// `POST /v1/corrections` — record a correction or retraction under the
/// server's writer identity, attributed to the key's user.
pub async fn post_correction(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<Json<CorrectionRequest>, JsonRejection>,
) -> Response {
    let principal = match reader_principal(&state, &headers) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let Json(req) = match body {
        Ok(b) => b,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, format!("body: {e}")),
    };
    let view = match load_view(&state, &principal).await {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let p = view.engine.projection();

    // Resolve the target to a session and the attrs the projector reads.
    let target = req.target.trim();
    let (kind, session_id, attrs): (EventKind, SessionId, Map<String, Value>) = match req
        .kind
        .trim()
    {
        "attempt_outcome" | "attempt_note" => {
            let Ok(aid) = target.parse::<AttemptId>() else {
                return error_response(StatusCode::BAD_REQUEST, "target must be an att_… id");
            };
            let Some(a) = p.attempts.iter().find(|a| a.attempt_id == aid) else {
                return error_response(
                    StatusCode::NOT_FOUND,
                    format!("no attempt {aid} in this tenant"),
                );
            };
            let mut attrs = Map::new();
            if req.kind.trim() == "attempt_outcome" {
                let Some(o) = req
                    .outcome
                    .as_deref()
                    .map(|o| o.trim().to_ascii_lowercase())
                else {
                    return error_response(
                        StatusCode::BAD_REQUEST,
                        "attempt_outcome needs `outcome`",
                    );
                };
                if !CORRECTABLE_OUTCOMES.contains(&o.as_str()) {
                    return error_response(
                        StatusCode::BAD_REQUEST,
                        format!(
                            "outcome {o:?}: expected one of {}",
                            CORRECTABLE_OUTCOMES.join(", ")
                        ),
                    );
                }
                attrs.insert(
                    "correction_type".into(),
                    json!(CorrectionType::AttemptOutcome.as_str()),
                );
                attrs.insert("outcome".into(), json!(o));
                if let Some(c) = req
                    .failure_class
                    .as_deref()
                    .map(str::trim)
                    .filter(|c| !c.is_empty())
                {
                    attrs.insert("failure_class".into(), json!(c));
                }
            } else {
                if req.note.as_deref().map(str::trim).unwrap_or("").is_empty() {
                    return error_response(StatusCode::BAD_REQUEST, "attempt_note needs `note`");
                }
                attrs.insert(
                    "correction_type".into(),
                    json!(CorrectionType::AttemptNote.as_str()),
                );
            }
            attrs.insert("target".into(), json!(format!("att_{aid}")));
            (EventKind::Correction, a.session_id, attrs)
        }
        "turn_objective" => {
            let Ok(tid) = target.parse::<TurnId>() else {
                return error_response(StatusCode::BAD_REQUEST, "target must be a trn_… id");
            };
            let Some(t) = p.turns.iter().find(|t| t.turn_id == tid) else {
                return error_response(
                    StatusCode::NOT_FOUND,
                    format!("no turn {tid} in this tenant"),
                );
            };
            if req.note.as_deref().map(str::trim).unwrap_or("").is_empty() {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "turn_objective needs `note` (the new objective)",
                );
            }
            let mut attrs = Map::new();
            attrs.insert(
                "correction_type".into(),
                json!(CorrectionType::TurnObjective.as_str()),
            );
            attrs.insert("target".into(), json!(format!("trn_{tid}")));
            (EventKind::Correction, t.session_id, attrs)
        }
        "retract_session" => {
            let Ok(sid) = target.parse::<SessionId>() else {
                return error_response(StatusCode::BAD_REQUEST, "target must be a ses_… id");
            };
            if p.session(sid).is_none() && !p.retracted.sessions.iter().any(|s| s.session_id == sid)
            {
                return error_response(
                    StatusCode::NOT_FOUND,
                    format!("no session {sid} in this tenant"),
                );
            }
            let reason = req.reason.as_deref().map(str::trim).unwrap_or("other");
            // The vocabulary is content-free; anything else folds to `other`.
            let reason = RetractionReason::parse(reason).as_str();
            let mut attrs = Map::new();
            attrs.insert(
                "target_type".into(),
                json!(RetractionTargetType::Session.as_str()),
            );
            attrs.insert("target".into(), json!(format!("ses_{sid}")));
            attrs.insert("reason".into(), json!(reason));
            (EventKind::Retraction, sid, attrs)
        }
        other => {
            return error_response(
                StatusCode::BAD_REQUEST,
                format!(
                    "type {other:?}: expected attempt_outcome, attempt_note, turn_objective or retract_session"
                ),
            );
        }
    };

    // A template event of the session gives the project and provider
    // session id the meta event must carry.
    let Some(first_id) = view
        .engine
        .session_event_ids_public(session_id)
        .into_iter()
        .next()
    else {
        return error_response(
            StatusCode::NOT_FOUND,
            format!("session {session_id} has no stored event"),
        );
    };
    let view2 = Arc::clone(&view);
    let template = tokio::task::spawn_blocking(move || find_event(&view2, first_id)).await;
    let Ok(Some(template)) = template else {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "cannot read the session's first event",
        );
    };

    let tenant = principal.tenant.clone();
    let user = principal.user_id.clone();
    let note = req.note.clone();
    let mode = state.config.capture_mode;
    let st = Arc::clone(&state);
    let written = tokio::task::spawn_blocking(move || -> anyhow::Result<(EventId, usize)> {
        let db = st.tenants.open(&tenant)?;
        let mut db = db
            .lock()
            .map_err(|_| anyhow::anyhow!("tenant {tenant}: database poisoned"))?;
        let mut ev = Event::new(
            db.device_id(),
            Provider::Other("attemptdb".into()),
            match kind {
                EventKind::Retraction => "Retraction",
                _ => "Correction",
            },
            kind,
            template.project.clone(),
            template.provider_session_id.clone(),
            mode,
            env!("CARGO_PKG_VERSION"),
        );
        ev.session_id = template.session_id;
        ev.agent.agent_id = AgentId::derive(&["session", &template.session_id.to_string()]);
        for (k, v) in attrs {
            ev.attrs.insert(k, v);
        }
        if let Some(u) = user {
            ev.attrs.insert("x_attemptdb_corrected_by".into(), json!(u));
        }
        if let Some(n) = note.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
            ev.attrs
                .insert("note_chars".into(), json!(n.chars().count() as u64));
            let mut extra = Map::new();
            extra.insert("note".into(), json!(n));
            ev.content = Some(attemptdb_core::event::EventContent {
                extra,
                ..Default::default()
            });
        }
        // Whatever the ceiling keeps: in metadata_only the note's text is
        // dropped here and only its length survives.
        ev.apply_capture_mode();
        let id = ev.event_id;
        let report = db.ingest(vec![ev])?;
        Ok((id, report.accepted))
    })
    .await;
    match written {
        Ok(Ok((id, accepted))) => {
            // Meta events are not activity: the live facts stay as they are.
            Json(json!({
                "tenant": principal.tenant.as_str(),
                "event_id": sh::id(&id),
                "kind": kind.as_str(),
                "session_id": sh::id(&session_id),
                "accepted": accepted,
                "note": "the observed facts are unchanged; the projection re-reads them through this event",
            }))
            .into_response()
        }
        Ok(Err(e)) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("cannot record: {e:#}"),
        ),
        Err(e) => error_response(StatusCode::SERVICE_UNAVAILABLE, format!("task failed: {e}")),
    }
}
