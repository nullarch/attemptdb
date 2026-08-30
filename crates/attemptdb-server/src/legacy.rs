//! `POST /v1/vibemon/hook` — the legacy VibeMon envelope (v2), one event per
//! request, exactly what `~/.vibemon/notify.sh` sends today. Existing
//! installs keep working by changing one URL; the event lands in the same
//! per-tenant database through the same ingest as `/v1/sync`.
//!
//! The device is the bearer key's device. The envelope carries no event id,
//! so one is minted here; the legacy client never retries, which is what
//! makes that safe.

use crate::AppState;
use attemptdb_adapters::vibemon::normalise_envelope;
use axum::Json;
use axum::extract::State;
use axum::extract::rejection::JsonRejection;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use std::sync::Arc;

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

pub async fn handle(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let Some(principal) = state.authenticate(authorization) else {
        return error(StatusCode::UNAUTHORIZED, "missing or unknown bearer key");
    };
    let Json(envelope) = match body {
        Ok(b) => b,
        Err(e) => return error(e.status(), e.body_text()),
    };
    let event = match normalise_envelope(principal.device_id, state.config.capture_mode, &envelope)
    {
        Ok(ev) => ev,
        Err(e) => return error(StatusCode::BAD_REQUEST, format!("envelope: {e}")),
    };
    let tenant = principal.tenant.clone();
    let st = Arc::clone(&state);
    let ingest = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let db = st.tenants.open(&tenant)?;
        let mut db = db
            .lock()
            .map_err(|_| anyhow::anyhow!("tenant {tenant}: database poisoned"))?;
        Ok(db.ingest(vec![event])?)
    })
    .await;
    match ingest {
        Ok(Ok(report)) => Json(json!({
            "accepted": report.accepted,
            "duplicates": report.duplicates,
            "redactions": report.redactions,
        }))
        .into_response(),
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
