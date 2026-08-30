//! Key issuance and revocation — the "device binding" half of sync.
//!
//! An operator (or the product's web app, server to server) calls these with
//! the admin token. The server mints a random key, returns it **once**, and
//! stores only its SHA-256 digest in the key file, which it rewrites
//! atomically and reloads. Nothing here is reachable without an admin token;
//! when none is configured the routes answer 404, as if they did not exist.
//!
//! `GET` lists digests, tenants, devices and labels — never keys.

use crate::AppState;
use crate::auth::{self, KeyEntry, Scope};
use crate::tenants::TenantId;
use attemptdb_core::DeviceId;
use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

/// Admin gate: 404 when no token is configured (the surface is absent), 401
/// on a wrong or missing bearer.
pub(crate) fn gate(state: &AppState, headers: &HeaderMap) -> Result<(), Box<Response>> {
    let Some(expected) = state.config.admin_token.as_deref() else {
        return Err(Box::new(error(StatusCode::NOT_FOUND, "not found")));
    };
    let presented = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, k)| k.trim());
    match presented {
        Some(k) if auth::eq_ct(k.as_bytes(), expected.as_bytes()) => Ok(()),
        _ => Err(Box::new(error(
            StatusCode::UNAUTHORIZED,
            "admin token required",
        ))),
    }
}

#[derive(Debug, Deserialize)]
pub struct IssueRequest {
    pub tenant: String,
    /// Omit to let the server mint a device id for a new device.
    #[serde(default)]
    pub device_id: Option<DeviceId>,
    #[serde(default)]
    pub label: String,
    /// `device` (default), `reader`, or `admin`. See [`Scope`].
    #[serde(default)]
    pub scope: Option<String>,
    /// The product's user this key belongs to; opaque, optional.
    #[serde(default)]
    pub user_id: Option<String>,
}

/// `POST /v1/admin/keys` — mint a key. The plaintext key is in the response
/// and nowhere else.
pub async fn issue(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<Json<IssueRequest>, JsonRejection>,
) -> Response {
    if let Err(r) = gate(&state, &headers) {
        return *r;
    }
    let Json(req) = match body {
        Ok(b) => b,
        Err(e) => return error(e.status(), e.body_text()),
    };
    let tenant = match TenantId::parse(&req.tenant) {
        Ok(t) => t,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.to_string()),
    };
    let scope = match req.scope.as_deref() {
        None => Scope::Device,
        Some(s) => match Scope::parse(s) {
            Some(s) => s,
            None => {
                return error(
                    StatusCode::BAD_REQUEST,
                    format!(
                        "unknown scope {s:?}; expected one of {}",
                        Scope::ALL
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                );
            }
        },
    };
    let user_id = match req.user_id.as_deref() {
        None => None,
        Some(u) => match auth::validate_user_id(u) {
            Ok(u) => Some(u),
            Err(e) => return error(StatusCode::BAD_REQUEST, e.to_string()),
        },
    };
    let device_id = req.device_id.unwrap_or_else(DeviceId::new);
    let key = auth::mint_key();
    let entry = KeyEntry {
        sha256: auth::digest_hex(&key),
        tenant: tenant.as_str().to_string(),
        device_id,
        label: req.label,
        scope,
        user_id,
    };
    let st = Arc::clone(&state);
    let added = entry.clone();
    let result = tokio::task::spawn_blocking(move || st.add_key(added)).await;
    match result {
        Ok(Ok(())) => (
            StatusCode::CREATED,
            Json(json!({
                "key": key,
                "sha256": entry.sha256,
                "tenant": entry.tenant,
                "device_id": entry.device_id,
                "label": entry.label,
                "scope": entry.scope.as_str(),
                "user_id": entry.user_id,
                "note": "store the key now; the server keeps only its digest",
            })),
        )
            .into_response(),
        Ok(Err(e)) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("cannot save the key file: {e:#}"),
        ),
        Err(e) => error(StatusCode::SERVICE_UNAVAILABLE, format!("task failed: {e}")),
    }
}

/// `GET /v1/admin/keys` — digests and bindings, never keys.
pub async fn list(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(r) = gate(&state, &headers) {
        return *r;
    }
    let entries: Vec<serde_json::Value> = state
        .keys
        .read()
        .map(|k| k.entries())
        .unwrap_or_default()
        .into_iter()
        .map(|e| {
            json!({
                "sha256": e.sha256,
                "tenant": e.tenant,
                "device_id": e.device_id,
                "label": e.label,
                "scope": e.scope.as_str(),
                "user_id": e.user_id,
            })
        })
        .collect();
    Json(json!({ "keys": entries })).into_response()
}

/// `DELETE /v1/admin/keys/{sha256}` — revoke.
pub async fn revoke(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(sha256): Path<String>,
) -> Response {
    if let Err(r) = gate(&state, &headers) {
        return *r;
    }
    let st = Arc::clone(&state);
    let digest = sha256.trim().to_ascii_lowercase();
    let result = tokio::task::spawn_blocking(move || st.remove_key(&digest)).await;
    match result {
        Ok(Ok(true)) => Json(json!({ "revoked": sha256 })).into_response(),
        Ok(Ok(false)) => error(StatusCode::NOT_FOUND, "no key with that digest"),
        Ok(Err(e)) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("cannot save the key file: {e:#}"),
        ),
        Err(e) => error(StatusCode::SERVICE_UNAVAILABLE, format!("task failed: {e}")),
    }
}

/// `POST /v1/admin/keys/reload` — re-read the key file (an operator edited
/// it by hand; the same thing SIGHUP does).
pub async fn reload(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Err(r) = gate(&state, &headers) {
        return *r;
    }
    let st = Arc::clone(&state);
    match tokio::task::spawn_blocking(move || st.reload_keys()).await {
        Ok(Ok(n)) => Json(json!({ "keys": n })).into_response(),
        Ok(Err(e)) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("cannot reload the key file: {e:#}"),
        ),
        Err(e) => error(StatusCode::SERVICE_UNAVAILABLE, format!("task failed: {e}")),
    }
}
