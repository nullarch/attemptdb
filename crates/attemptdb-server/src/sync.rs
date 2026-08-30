//! `POST /v1/sync` — one upload batch in, one acknowledgement out.
//!
//! The batch is RFC 0006 §10.3 with `events` as RFC 0001 canonical envelopes
//! — the same JSON `attempt hook` spools locally, so a client has nothing to
//! translate. Idempotency is the engine's: `event_id` is minted by the
//! client, ingest deduplicates by it, and a re-sent batch acknowledges the
//! same events as duplicates instead of storing them twice.

use crate::AppState;
use crate::auth::Principal;
use attemptdb_core::{CaptureMode, DeviceId, Event, EventId};
use axum::Json;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;

pub const SYNC_VERSION: u32 = 1;

/// Most events one batch may carry. A client with more splits them; the
/// order across batches is preserved by sending one batch at a time.
pub const MAX_BATCH_EVENTS: usize = 5_000;

#[derive(Debug, Deserialize)]
pub struct SyncBatch {
    pub sync_version: u32,
    pub device_id: DeviceId,
    /// Client-chosen; echoed back so an ack can be matched to its batch.
    pub batch_id: String,
    /// What the client believes it is allowed to persist. Informational: the
    /// server's mode is the ceiling regardless.
    #[serde(default)]
    pub capture_mode: Option<CaptureMode>,
    pub events: Vec<Event>,
}

#[derive(Debug, Serialize)]
pub struct SyncAck {
    pub sync_version: u32,
    pub batch_id: String,
    /// Stored for the first time.
    pub accepted: usize,
    /// Already stored (a re-sent batch, or overlapping batches).
    pub duplicates: usize,
    /// Not stored, with the reason; the client should not retry these.
    pub rejected: Vec<Rejected>,
    /// Attrs dropped by the engine's contract check across the batch.
    pub redactions: usize,
    /// Events whose `content`/`raw` were removed by the server's capture
    /// mode ceiling before storage.
    pub stripped_content: usize,
}

#[derive(Debug, Serialize)]
pub struct Rejected {
    pub event_id: EventId,
    pub reason: &'static str,
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

fn rank(mode: CaptureMode) -> u8 {
    match mode {
        CaptureMode::MetadataOnly => 0,
        CaptureMode::LocalSemantic => 1,
        CaptureMode::FullSync => 2,
    }
}

/// The more restrictive of the two.
pub fn clamp(client: CaptureMode, ceiling: CaptureMode) -> CaptureMode {
    if rank(client) <= rank(ceiling) {
        client
    } else {
        ceiling
    }
}

pub async fn handle(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<Json<SyncBatch>, JsonRejection>,
) -> Response {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let Some(principal) = state.authenticate(authorization) else {
        return error(StatusCode::UNAUTHORIZED, "missing or unknown bearer key");
    };
    let Json(batch) = match body {
        Ok(b) => b,
        Err(e) => return error(e.status(), e.body_text()),
    };
    if batch.sync_version != SYNC_VERSION {
        return error(
            StatusCode::BAD_REQUEST,
            format!(
                "sync_version {} not supported (server speaks {SYNC_VERSION})",
                batch.sync_version
            ),
        );
    }
    if batch.device_id != principal.device_id {
        return error(
            StatusCode::FORBIDDEN,
            "batch device_id does not match the key's device",
        );
    }
    if batch.events.len() > MAX_BATCH_EVENTS {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "{} events in one batch; the limit is {MAX_BATCH_EVENTS}",
                batch.events.len()
            ),
        );
    }

    let (events, mut rejected, stripped_content) =
        prepare(batch.events, &principal, state.config.capture_mode);
    let batch_id = batch.batch_id;

    if events.is_empty() {
        return Json(SyncAck {
            sync_version: SYNC_VERSION,
            batch_id,
            accepted: 0,
            duplicates: 0,
            rejected,
            redactions: 0,
            stripped_content,
        })
        .into_response();
    }

    let tenant = principal.tenant.clone();
    let st = Arc::clone(&state);
    let ingest = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let db = st.tenants.open(&tenant)?;
        let mut db = db
            .lock()
            .map_err(|_| anyhow::anyhow!("tenant {tenant}: database poisoned"))?;
        Ok(db.ingest(events)?)
    })
    .await;
    match ingest {
        Ok(Ok(report)) => Json(SyncAck {
            sync_version: SYNC_VERSION,
            batch_id,
            accepted: report.accepted,
            duplicates: report.duplicates,
            rejected: std::mem::take(&mut rejected),
            redactions: report.redactions,
            stripped_content,
        })
        .into_response(),
        // Storage trouble is the server's, not the client's: say so with a
        // status that tells the client to keep the batch and retry.
        Ok(Err(e)) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("ingest failed: {e:#}"),
        ),
        Err(e) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("ingest task failed: {e}"),
        ),
    }
}

/// Per-event checks and the capture-mode ceiling. Returns the events to
/// ingest, the rejections, and how many events lost content to the ceiling.
fn prepare(
    events: Vec<Event>,
    principal: &Principal,
    ceiling: CaptureMode,
) -> (Vec<Event>, Vec<Rejected>, usize) {
    let mut keep = Vec::with_capacity(events.len());
    let mut rejected = Vec::new();
    let mut stripped = 0;
    for mut ev in events {
        if ev.device_id != principal.device_id {
            rejected.push(Rejected {
                event_id: ev.event_id,
                reason: "event device_id does not match the batch",
            });
            continue;
        }
        // The client's own sequence number survives as metadata; the server
        // assigns this database's `source_seq` at ingest.
        if ev.source_seq != 0 {
            ev.attrs
                .insert("device_seq".to_string(), json!(ev.source_seq));
        }
        let had_content = ev.content.is_some() || ev.raw.is_some();
        ev.capture_mode = clamp(ev.capture_mode, ceiling);
        ev.apply_capture_mode();
        if had_content && !ev.capture_mode.persists_content_locally() {
            stripped += 1;
        }
        keep.push(ev);
    }
    (keep, rejected, stripped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_is_the_minimum() {
        use CaptureMode::*;
        assert_eq!(clamp(FullSync, MetadataOnly), MetadataOnly);
        assert_eq!(clamp(MetadataOnly, FullSync), MetadataOnly);
        assert_eq!(clamp(LocalSemantic, LocalSemantic), LocalSemantic);
        assert_eq!(clamp(LocalSemantic, FullSync), LocalSemantic);
    }
}
