//! `DELETE /v1/admin/devices/{device_id}` — a device leaves.
//!
//! Two things happen, in this order: every device key bound to it is
//! revoked (its next upload gets 401), then, in each tenant concerned, one
//! Retraction is recorded per session that device wrote (RFC 0003 §8,
//! reason `revoked`). The facts stay in the tenant's segments — the log is
//! immutable — but every projection behaves as if those sessions never
//! happened, which is what "this person left the organisation" means for a
//! team timeline. Removing the bytes is a retention decision, not an API
//! call.
//!
//! Which tenants: those where the device holds a key, or the one named by
//! `?tenant=` (which also lets an operator retract a device whose keys were
//! already revoked). A repeat call is safe: sessions already retracted are
//! counted, not retracted twice.

use crate::AppState;
use crate::admin::gate;
use crate::auth::Scope;
use crate::tenants::TenantId;
use anyhow::Result;
use attemptdb_core::event::Provider;
use attemptdb_core::{AgentId, CaptureMode, DeviceId, Event, EventKind, SessionId};
use attemptdb_project::{RetractionReason, RetractionTargetType, is_meta_kind, retracted_ids};
use attemptdb_storage::ScanFilter;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

#[derive(Debug, Deserialize)]
pub struct DeleteParams {
    /// Act on this tenant only (and on it even when the device holds no key
    /// there any more). Default: every tenant where the device has a key.
    #[serde(default)]
    pub tenant: Option<String>,
}

/// What happened in one tenant.
#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct TenantOutcome {
    pub tenant: String,
    pub keys_revoked: usize,
    /// Sessions of the device retracted by this call.
    pub sessions_retracted: usize,
    /// Sessions of the device that an earlier call (or an operator) had
    /// already retracted.
    pub sessions_already_retracted: usize,
    /// Non-meta events the device wrote to this tenant, across all its
    /// sessions; they stay on disk.
    pub events_affected: usize,
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

/// The Retraction the server writes for one session of a departing device.
/// Mirrors what `attempt retract --session … --reason revoked` writes on a
/// device, minus the free-text note: `provider = attemptdb`, the target
/// session's own id and project, and the content-free reason vocabulary.
pub fn retraction_event(server_device: DeviceId, template: &Event, mode: CaptureMode) -> Event {
    let mut ev = Event::new(
        server_device,
        Provider::Other("attemptdb".into()),
        "Retraction",
        EventKind::Retraction,
        template.project.clone(),
        template.provider_session_id.clone(),
        mode,
        env!("CARGO_PKG_VERSION"),
    );
    ev.session_id = template.session_id;
    ev.agent.agent_id = AgentId::derive(&["session", &template.session_id.to_string()]);
    ev.attrs.insert(
        "target_type".into(),
        Value::from(RetractionTargetType::Session.as_str()),
    );
    ev.attrs.insert(
        "target".into(),
        Value::from(format!("ses_{}", template.session_id)),
    );
    ev.attrs.insert(
        "reason".into(),
        Value::from(RetractionReason::Revoked.as_str()),
    );
    ev.apply_capture_mode();
    ev
}

/// Retract every session `device` wrote in `tenant`. Blocking.
fn retract_in_tenant(
    state: &AppState,
    tenant: &TenantId,
    device: DeviceId,
) -> Result<(usize, usize, usize)> {
    let db = state.tenants.open(tenant)?;
    let mut db = db
        .lock()
        .map_err(|_| anyhow::anyhow!("tenant {tenant}: database poisoned"))?;
    let events = db.scan(&ScanFilter::default())?;
    let already = retracted_ids(&events);
    let mut sessions: BTreeMap<SessionId, &Event> = BTreeMap::new();
    let mut affected = 0usize;
    for e in events
        .iter()
        .filter(|e| e.device_id == device && !is_meta_kind(e.kind))
    {
        affected += 1;
        sessions.entry(e.session_id).or_insert(e);
    }
    let server_device = db.device_id();
    let mut retractions = Vec::new();
    let mut skipped = 0usize;
    for (sid, template) in &sessions {
        if already.contains_session(sid) {
            skipped += 1;
            continue;
        }
        retractions.push(retraction_event(
            server_device,
            template,
            state.config.capture_mode,
        ));
    }
    let written = retractions.len();
    if written > 0 {
        db.ingest(retractions)?;
    }
    Ok((written, skipped, affected))
}

/// `DELETE /v1/admin/devices/{device_id}[?tenant=…]`.
pub async fn delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(device_id): Path<DeviceId>,
    Query(params): Query<DeleteParams>,
) -> Response {
    if let Err(r) = gate(&state, &headers) {
        return *r;
    }
    let only = match params.tenant.as_deref() {
        None => None,
        Some(t) => match TenantId::parse(t) {
            Ok(t) => Some(t),
            Err(e) => return error(StatusCode::BAD_REQUEST, e.to_string()),
        },
    };
    let entries = state.keys.read().map(|k| k.entries()).unwrap_or_default();
    let mut tenants: BTreeSet<String> = entries
        .iter()
        .filter(|e| e.device_id == device_id && e.scope == Scope::Device)
        .filter(|e| only.as_ref().is_none_or(|t| t.as_str() == e.tenant))
        .map(|e| e.tenant.clone())
        .collect();
    if let Some(t) = &only {
        tenants.insert(t.as_str().to_string());
    }
    if tenants.is_empty() {
        return error(
            StatusCode::NOT_FOUND,
            "no device key bound to that device; pass ?tenant=<id> to retract its sessions there anyway",
        );
    }

    let st = Arc::clone(&state);
    let outcome = tokio::task::spawn_blocking(move || -> Result<Vec<TenantOutcome>> {
        let mut out = Vec::new();
        for name in tenants {
            let tenant = TenantId::parse(&name)?;
            let keys_revoked = st.remove_keys_where(|e| {
                !(e.device_id == device_id && e.scope == Scope::Device && e.tenant == name)
            })?;
            let (retracted, already, affected) = retract_in_tenant(&st, &tenant, device_id)?;
            out.push(TenantOutcome {
                tenant: name,
                keys_revoked,
                sessions_retracted: retracted,
                sessions_already_retracted: already,
                events_affected: affected,
            });
        }
        Ok(out)
    })
    .await;
    match outcome {
        Ok(Ok(tenants)) => {
            let keys_revoked: usize = tenants.iter().map(|t| t.keys_revoked).sum();
            Json(json!({
                "device_id": device_id,
                "keys_revoked": keys_revoked,
                "tenants": tenants,
                "note": "facts stay on disk; the retracted sessions leave every projection",
            }))
            .into_response()
        }
        Ok(Err(e)) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("device removal failed: {e:#}"),
        ),
        Err(e) => error(StatusCode::SERVICE_UNAVAILABLE, format!("task failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use attemptdb_core::ProjectRef;

    #[test]
    fn retraction_targets_the_template_session_with_the_revoked_reason() {
        let dev = DeviceId::derive(&["t", "d"]);
        let template = Event::new(
            dev,
            Provider::ClaudeCode,
            "PostToolUse",
            EventKind::ToolCallFinished,
            ProjectRef::derive("/home/dev/example/project", None, &dev),
            "session-1",
            CaptureMode::LocalSemantic,
            "test/0",
        );
        let server = DeviceId::derive(&["attemptdb-server", "alpha"]);
        let r = retraction_event(server, &template, CaptureMode::MetadataOnly);
        assert_eq!(r.kind, EventKind::Retraction);
        assert_eq!(r.device_id, server);
        assert_eq!(r.session_id, template.session_id);
        assert_eq!(r.attrs["target_type"], "session");
        assert_eq!(
            r.attrs["target"],
            format!("ses_{}", template.session_id).as_str()
        );
        assert_eq!(r.attrs["reason"], "revoked");
        assert!(r.content.is_none() && r.raw.is_none());
        assert!(is_meta_kind(r.kind));
    }
}
