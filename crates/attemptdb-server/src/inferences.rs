//! `POST /v1/sync/inferences` and `GET /v1/inferences` — device-computed
//! projections (RFC 0006 §10.7).
//!
//! Inferences are not facts (RFC 0003), so they are never ingested into the
//! tenant's event database. They are stored beside it, one document per
//! `(device, kind)`, replaced wholesale on every upload, and every item must
//! carry provenance: evidence event ids, a confidence in `[0, 1]`, and the
//! algorithm version. Items without it are rejected by id. Under a
//! `metadata_only` ceiling the content-bearing fields (`objective`,
//! `rationale`) are removed before the document is written, the same rule
//! the event path applies to `content`/`raw`.

use crate::AppState;
use attemptdb_core::{CaptureMode, DeviceId, Timestamp};
use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const SYNC_VERSION: u32 = 1;
pub const INFERENCE_SCHEMA: &str = "attemptdb.inference/v1";
pub const KINDS: &[&str] = &["attempt", "handoff", "work_unit", "decision"];
pub const MAX_ITEMS: usize = 20_000;
const CONTENT_FIELDS: &[&str] = &["objective", "rationale"];

#[derive(Debug, Deserialize)]
pub struct InferenceBatch {
    pub sync_version: u32,
    pub schema: String,
    pub device_id: DeviceId,
    pub batch_id: String,
    pub kind: String,
    pub algorithm_version: String,
    pub computed_at: Timestamp,
    pub items: Vec<Value>,
}

#[derive(Debug, Serialize)]
pub struct RejectedItem {
    pub id: String,
    pub reason: &'static str,
}

#[derive(Debug, Serialize)]
pub struct InferenceAck {
    pub sync_version: u32,
    pub batch_id: String,
    pub kind: String,
    pub stored: usize,
    pub rejected: Vec<RejectedItem>,
    /// Content-bearing fields removed by the server's capture-mode ceiling.
    pub stripped: usize,
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

/// Provenance rules every item must satisfy.
pub fn validate(item: &Value) -> Result<(), &'static str> {
    let Some(obj) = item.as_object() else {
        return Err("not an object");
    };
    match obj.get("id").and_then(Value::as_str) {
        Some(id) if !id.trim().is_empty() => {}
        _ => return Err("missing id"),
    }
    match obj.get("evidence").and_then(Value::as_array) {
        Some(ev) if !ev.is_empty() && ev.iter().all(Value::is_string) => {}
        Some(_) => return Err("evidence must be a non-empty list of event ids"),
        None => return Err("missing evidence"),
    }
    match obj.get("confidence").and_then(Value::as_f64) {
        Some(c) if (0.0..=1.0).contains(&c) => {}
        _ => return Err("confidence must be a number in [0, 1]"),
    }
    match obj.get("algorithm_version").and_then(Value::as_str) {
        Some(v) if !v.trim().is_empty() => {}
        _ => return Err("missing algorithm_version"),
    }
    if !obj.get("fields").is_some_and(Value::is_object) {
        return Err("fields must be an object");
    }
    Ok(())
}

/// Null out content-bearing fields; returns how many held a value.
pub fn strip_content(item: &mut Value) -> usize {
    let Some(fields) = item.get_mut("fields").and_then(Value::as_object_mut) else {
        return 0;
    };
    let mut n = 0;
    for key in CONTENT_FIELDS {
        if let Some(v) = fields.get_mut(*key)
            && !v.is_null()
        {
            *v = Value::Null;
            n += 1;
        }
    }
    n
}

/// `<tenant>/inferences/<device_id>/<kind>.json`.
pub fn store_path(tenant_dir: &Path, device_id: &DeviceId, kind: &str) -> PathBuf {
    tenant_dir
        .join("inferences")
        .join(device_id.to_string())
        .join(format!("{kind}.json"))
}

fn write_atomically(path: &Path, doc: &Value) -> anyhow::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent", path.display()))?;
    std::fs::create_dir_all(dir)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(doc)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub async fn handle(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Result<Json<InferenceBatch>, JsonRejection>,
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
    if batch.schema != INFERENCE_SCHEMA {
        return error(
            StatusCode::BAD_REQUEST,
            format!(
                "schema {:?} not supported (server speaks {INFERENCE_SCHEMA})",
                batch.schema
            ),
        );
    }
    if batch.device_id != principal.device_id {
        return error(
            StatusCode::FORBIDDEN,
            "batch device_id does not match the key's device",
        );
    }
    if !KINDS.contains(&batch.kind.as_str()) {
        return error(
            StatusCode::BAD_REQUEST,
            format!(
                "kind {:?} not supported (one of {})",
                batch.kind,
                KINDS.join(", ")
            ),
        );
    }
    if batch.items.len() > MAX_ITEMS {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!(
                "{} items in one upload; the limit is {MAX_ITEMS}",
                batch.items.len()
            ),
        );
    }
    if batch.algorithm_version.trim().is_empty() {
        return error(StatusCode::BAD_REQUEST, "algorithm_version is empty");
    }

    let strip = state.config.capture_mode == CaptureMode::MetadataOnly;
    let mut kept = Vec::with_capacity(batch.items.len());
    let mut rejected = Vec::new();
    let mut stripped = 0;
    for mut item in batch.items {
        if let Err(reason) = validate(&item) {
            let id = item
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            rejected.push(RejectedItem { id, reason });
            continue;
        }
        if strip {
            stripped += strip_content(&mut item);
        }
        kept.push(item);
    }
    let stored = kept.len();
    let doc = json!({
        "schema": INFERENCE_SCHEMA,
        "device_id": batch.device_id,
        "kind": batch.kind,
        "algorithm_version": batch.algorithm_version,
        "computed_at": batch.computed_at,
        "received_at": Timestamp::now(),
        "items": kept,
    });
    let path = store_path(
        &state.tenants.dir(&principal.tenant),
        &principal.device_id,
        &batch.kind,
    );
    let written = tokio::task::spawn_blocking(move || write_atomically(&path, &doc)).await;
    match written {
        Ok(Ok(())) => Json(InferenceAck {
            sync_version: SYNC_VERSION,
            batch_id: batch.batch_id,
            kind: batch.kind,
            stored,
            rejected,
            stripped,
        })
        .into_response(),
        Ok(Err(e)) => error(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("cannot store inferences: {e:#}"),
        ),
        Err(e) => error(StatusCode::SERVICE_UNAVAILABLE, format!("task failed: {e}")),
    }
}

#[derive(Debug, Deserialize)]
pub struct GetParams {
    #[serde(default)]
    pub kind: Option<String>,
}

/// `GET /v1/inferences[?kind=attempt]` — the key's own device: one stored
/// document, or a summary of every kind present.
pub async fn get(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(params): Query<GetParams>,
) -> Response {
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    let Some(principal) = state.authenticate(authorization) else {
        return error(StatusCode::UNAUTHORIZED, "missing or unknown bearer key");
    };
    let tenant_dir = state.tenants.dir(&principal.tenant);
    let device_id = principal.device_id;
    let read = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        let load = |kind: &str| -> anyhow::Result<Option<Value>> {
            match std::fs::read(store_path(&tenant_dir, &device_id, kind)) {
                Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(e.into()),
            }
        };
        match params.kind.as_deref() {
            Some(kind) => {
                if !KINDS.contains(&kind) {
                    anyhow::bail!("kind {kind:?} not supported");
                }
                Ok(load(kind)?.unwrap_or(Value::Null))
            }
            None => {
                let mut kinds = Vec::new();
                for kind in KINDS {
                    if let Some(doc) = load(kind)? {
                        kinds.push(json!({
                            "kind": kind,
                            "items": doc.get("items").and_then(Value::as_array).map_or(0, Vec::len),
                            "algorithm_version": doc.get("algorithm_version"),
                            "computed_at": doc.get("computed_at"),
                            "received_at": doc.get("received_at"),
                        }));
                    }
                }
                Ok(json!({ "device_id": device_id, "kinds": kinds }))
            }
        }
    })
    .await;
    match read {
        Ok(Ok(Value::Null)) => error(
            StatusCode::NOT_FOUND,
            "no inferences of that kind stored for this device",
        ),
        Ok(Ok(doc)) => Json(doc).into_response(),
        Ok(Err(e)) => error(StatusCode::BAD_REQUEST, format!("{e:#}")),
        Err(e) => error(StatusCode::SERVICE_UNAVAILABLE, format!("task failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(ev: usize, conf: f64) -> Value {
        json!({
            "kind": "attempt",
            "id": "att_1",
            "evidence": (0..ev).map(|i| format!("evt_{i}")).collect::<Vec<_>>(),
            "confidence": conf,
            "algorithm_version": "tier1-v0",
            "fields": { "objective": "fix the build", "approach": "edit src/lib.rs" }
        })
    }

    #[test]
    fn provenance_is_required() {
        assert_eq!(validate(&item(2, 0.9)), Ok(()));
        assert_eq!(
            validate(&item(0, 0.9)),
            Err("evidence must be a non-empty list of event ids")
        );
        assert_eq!(
            validate(&item(1, 1.5)),
            Err("confidence must be a number in [0, 1]")
        );
        let mut no_algo = item(1, 0.5);
        no_algo["algorithm_version"] = json!("");
        assert_eq!(validate(&no_algo), Err("missing algorithm_version"));
        let mut no_id = item(1, 0.5);
        no_id.as_object_mut().unwrap().remove("id");
        assert_eq!(validate(&no_id), Err("missing id"));
        assert_eq!(validate(&json!("x")), Err("not an object"));
    }

    #[test]
    fn content_fields_are_stripped_and_the_rest_kept() {
        let mut it = item(1, 0.9);
        assert_eq!(strip_content(&mut it), 1);
        assert!(it["fields"]["objective"].is_null());
        assert_eq!(it["fields"]["approach"], json!("edit src/lib.rs"));
        assert_eq!(strip_content(&mut it), 0);
    }
}
