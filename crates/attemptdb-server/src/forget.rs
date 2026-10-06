//! Taking back what a device uploaded.
//!
//! Three routes, all of which end something the device (or its operator)
//! started, and none of which exist for a reader key:
//!
//! - `POST /v1/sync/forget` (device key, body `{"confirm": true}`) — delete
//!   every event this device uploaded to its tenant. The key stays valid: the
//!   device may keep syncing from where its cursor is.
//! - `POST /v1/sync/revoke` (device key) — revoke the presenting key. What the
//!   device already uploaded stays unless it was forgotten first
//!   (`attempt sync disconnect --forget` does both, in that order).
//! - `DELETE /v1/admin/devices/{device_id}/events[?tenant=…]` (admin token) —
//!   the operator's version of the first: for a device whose key is gone, or
//!   one that cannot ask for itself.
//!
//! "Delete" is the storage engine's purge: every segment holding a row of the
//! device is rewritten without it (one manifest generation per segment, the
//! compaction protocol), the old file is tombstoned, and a deletion record —
//! a `config_changed` event carrying the count and the reason, never the
//! deleted content (RFC 0006 §8) — is written afterwards, which moves the
//! manifest on one more generation so the last tombstoned file is removed too.
//! The device's stored inference documents go with it, and the tenant's live
//! facts are dropped so the next read rebuilds them from what is left.
//!
//! What this does **not** reach, and the response says so: copies the
//! product already received through the webhook, filesystem snapshots and
//! backups of the volume, and the operator's own logs. A tenant database that
//! holds encrypted content blobs is refused (a purge keeps blob files; the
//! hosted server stores content inline, so this does not arise there) rather
//! than half-deleted.

use crate::AppState;
use crate::admin::gate;
use crate::auth::{self, Principal, Scope};
use crate::tenants::{TenantId, writer_device_id};
use anyhow::Result;
use attemptdb_core::event::Provider;
use attemptdb_core::{CaptureMode, DeviceId, Event, EventKind, ProjectRef};
use attemptdb_storage::PurgeReport;
use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

/// What stays after a deletion, said in every response.
pub const NOT_REACHED: &[&str] = &[
    "events the product already received through the webhook",
    "backups and filesystem snapshots of the server's volume",
    "the operator's own logs",
];

/// What one deletion did in one tenant.
#[derive(Debug, Default, Serialize, PartialEq, Eq)]
pub struct ForgetOutcome {
    pub tenant: String,
    pub device_id: String,
    pub events_deleted: u64,
    /// Rows left in the tenant (other devices', and the server's own).
    pub events_kept: u64,
    pub segments_rewritten: u64,
    pub segments_removed: u64,
    /// Stored inference documents removed with the events.
    pub inference_documents_removed: usize,
    pub generation: u64,
}

/// Why a deletion did not happen.
#[derive(Debug)]
pub enum ForgetError {
    /// The tenant database holds encrypted blobs, which a purge does not remove.
    HasBlobs,
    Failed(anyhow::Error),
}

impl From<anyhow::Error> for ForgetError {
    fn from(e: anyhow::Error) -> Self {
        ForgetError::Failed(e)
    }
}

/// The deletion record: a `config_changed` event from the server's writer.
/// Counts and a reason from a closed vocabulary; nothing of what was removed.
pub fn deletion_record(
    tenant: &TenantId,
    device: DeviceId,
    deleted: u64,
    requested_by: &'static str,
) -> Event {
    let writer = writer_device_id(tenant);
    let mut ev = Event::new(
        writer,
        Provider::Other("attemptdb".into()),
        "Deletion",
        EventKind::ConfigChanged,
        ProjectRef::derive("attemptdb/server", None, &writer),
        "attemptdb-deletions",
        CaptureMode::MetadataOnly,
        env!("CARGO_PKG_VERSION"),
    );
    ev.attrs
        .insert("x_attemptdb_deletion_reason".into(), json!(requested_by));
    ev.attrs
        .insert("x_attemptdb_events_deleted".into(), json!(deleted));
    ev.attrs
        .insert("x_attemptdb_device".into(), json!(device.to_string()));
    ev
}

/// Delete every event `device` uploaded to `tenant`, and what was derived
/// from them on the server. Blocking: the tenant's writer is held one
/// rewritten segment at a time, so its uploads and reads interleave.
pub fn forget_device(
    state: &AppState,
    tenant: &TenantId,
    device: DeviceId,
    requested_by: &'static str,
) -> Result<ForgetOutcome, ForgetError> {
    if !state.tenants.dir(tenant).exists() {
        return Ok(ForgetOutcome {
            tenant: tenant.as_str().to_string(),
            device_id: device.to_string(),
            ..Default::default()
        });
    }
    let handle = state.tenants.open(tenant)?;
    {
        let db = handle
            .lock()
            .map_err(|_| anyhow::anyhow!("tenant {tenant}: database poisoned"))?;
        if !db.blob_store().is_empty().map_err(anyhow::Error::from)? {
            return Err(ForgetError::HasBlobs);
        }
    }
    let keep = move |e: &Event| e.device_id != device;
    let mut total = PurgeReport::default();
    let mut clean = HashSet::new();
    loop {
        let (slice, done) = {
            let mut db = handle
                .lock()
                .map_err(|_| anyhow::anyhow!("tenant {tenant}: database poisoned"))?;
            db.purge_some(&keep, attemptdb_storage::PURGE_CHUNK_ROWS, 1, &clean)
                .map_err(anyhow::Error::from)?
        };
        clean.extend(slice.clean_segments.iter().copied());
        total.absorb(slice);
        if done {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    if total.events_dropped > 0 {
        // The record is the audit trail, and its flush is one more manifest
        // generation: the file the last rewrite tombstoned is deleted by it.
        let mut db = handle
            .lock()
            .map_err(|_| anyhow::anyhow!("tenant {tenant}: database poisoned"))?;
        db.ingest(vec![deletion_record(
            tenant,
            device,
            total.events_dropped,
            requested_by,
        )])
        .map_err(anyhow::Error::from)?;
        db.flush().map_err(anyhow::Error::from)?;
        db.collect_garbage().map_err(anyhow::Error::from)?;
    }
    let docs = state
        .tenants
        .dir(tenant)
        .join("inferences")
        .join(device.to_string());
    let removed_docs = match std::fs::read_dir(&docs) {
        Ok(rd) => {
            let n = rd.filter_map(|e| e.ok()).count();
            std::fs::remove_dir_all(&docs).map_err(anyhow::Error::from)?;
            n
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
        Err(e) => return Err(ForgetError::Failed(e.into())),
    };
    state.live.forget(tenant);
    eprintln!(
        "tenant {tenant}: forgot device {device} ({requested_by}): dropped {} of {} row(s), {} inference document(s); generation {}",
        total.events_dropped,
        total.events_dropped + total.events_kept,
        removed_docs,
        total.generation
    );
    Ok(ForgetOutcome {
        tenant: tenant.as_str().to_string(),
        device_id: device.to_string(),
        events_deleted: total.events_dropped,
        events_kept: total.events_kept,
        segments_rewritten: total.segments_rewritten,
        segments_removed: total.segments_removed,
        inference_documents_removed: removed_docs,
        generation: total.generation,
    })
}

fn forget_error(e: ForgetError) -> Response {
    match e {
        ForgetError::HasBlobs => error(
            StatusCode::CONFLICT,
            "this tenant's database holds encrypted content blobs, which a deletion would leave \
             on disk; refusing a half-deletion — ask the operator",
        ),
        ForgetError::Failed(e) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("deletion failed: {e:#}"),
        ),
    }
}

/// A device key, and only a device key: 401 for an unknown key, 403 for a
/// reader or admin one.
fn device_principal(state: &AppState, headers: &HeaderMap) -> Result<Principal, Box<Response>> {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let Some(principal) = state.authenticate(authorization) else {
        return Err(Box::new(error(
            StatusCode::UNAUTHORIZED,
            "missing or unknown bearer key",
        )));
    };
    if !principal.can_write() {
        return Err(Box::new(error(
            StatusCode::FORBIDDEN,
            format!(
                "a {} key cannot do this; it needs the device's own key",
                principal.scope.as_str()
            ),
        )));
    }
    Ok(principal)
}

#[derive(Debug, Deserialize)]
pub struct ForgetRequest {
    /// Must be `true`: deletion is not something an empty body does.
    #[serde(default)]
    pub confirm: bool,
}

/// `POST /v1/sync/forget`.
pub async fn device_forget(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<Json<ForgetRequest>, JsonRejection>,
) -> Response {
    let principal = match device_principal(&state, &headers) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let confirmed = matches!(body, Ok(Json(ForgetRequest { confirm: true })));
    if !confirmed {
        return error(
            StatusCode::BAD_REQUEST,
            "this deletes everything the device uploaded; send {\"confirm\": true}",
        );
    }
    let st = Arc::clone(&state);
    let (tenant, device) = (principal.tenant.clone(), principal.device_id);
    let result =
        tokio::task::spawn_blocking(move || forget_device(&st, &tenant, device, "device")).await;
    match result {
        Ok(Ok(outcome)) => Json(json!({
            "forgotten": true,
            "outcome": outcome,
            "not_reached": NOT_REACHED,
        }))
        .into_response(),
        Ok(Err(e)) => forget_error(e),
        Err(e) => error(StatusCode::SERVICE_UNAVAILABLE, format!("task failed: {e}")),
    }
}

/// `POST /v1/sync/revoke` — the presenting key is revoked; its next request
/// gets 401.
pub async fn device_revoke(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    let principal = match device_principal(&state, &headers) {
        Ok(p) => p,
        Err(r) => return *r,
    };
    let Some(key) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once(' '))
        .map(|(_, k)| k.trim().to_string())
    else {
        return error(StatusCode::UNAUTHORIZED, "missing or unknown bearer key");
    };
    let digest = auth::digest_hex(&key);
    let st = Arc::clone(&state);
    match tokio::task::spawn_blocking(move || st.remove_key(&digest)).await {
        Ok(Ok(removed)) => Json(json!({
            "revoked": removed,
            "tenant": principal.tenant.as_str(),
            "device_id": principal.device_id,
            "kept_on_server": "what this device already uploaded stays on the server unless it was \
                               forgotten first (POST /v1/sync/forget)",
        }))
        .into_response(),
        Ok(Err(e)) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("cannot revoke: {e:#}"),
        ),
        Err(e) => error(StatusCode::SERVICE_UNAVAILABLE, format!("task failed: {e}")),
    }
}

#[derive(Debug, Deserialize)]
pub struct AdminForgetParams {
    /// Act on this tenant only (default: every tenant where the device holds
    /// a device key).
    #[serde(default)]
    pub tenant: Option<String>,
}

/// `DELETE /v1/admin/devices/{device_id}/events[?tenant=…]`.
pub async fn admin_forget(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(device_id): Path<DeviceId>,
    Query(params): Query<AdminForgetParams>,
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
            "no device key bound to that device; pass ?tenant=<id> to delete its events there anyway",
        );
    }
    let st = Arc::clone(&state);
    let result = tokio::task::spawn_blocking(move || -> Result<Vec<ForgetOutcome>, ForgetError> {
        let mut out = Vec::new();
        for name in tenants {
            let tenant = TenantId::parse(&name)?;
            out.push(forget_device(&st, &tenant, device_id, "operator")?);
        }
        Ok(out)
    })
    .await;
    match result {
        Ok(Ok(outcomes)) => Json(json!({
            "device_id": device_id,
            "events_deleted": outcomes.iter().map(|o| o.events_deleted).sum::<u64>(),
            "tenants": outcomes,
            "not_reached": NOT_REACHED,
        }))
        .into_response(),
        Ok(Err(e)) => forget_error(e),
        Err(e) => error(StatusCode::SERVICE_UNAVAILABLE, format!("task failed: {e}")),
    }
}
